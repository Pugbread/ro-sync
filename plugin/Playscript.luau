--!nonstrict

-- Long-lived, generation-scoped playtest scripts for Ro Sync.
--
-- This module deliberately owns the playscript runtime and coordinator state so
-- the already-large Plugin.luau chunk does not cross Luau's 200-register limit.

local Playscript = {}

local FRAME_KIND = "rosync-playscript-frame"
local EVENT_BYTES = 64 * 1024
local RESULT_BYTES = 1024 * 1024
local RESULT_CHUNK_BYTES = 96 * 1024
local RESULT_TTL_SECONDS = 120
local RESULT_MAX_SESSIONS = 16
local HEARTBEAT_SECONDS = 2
local EVENT_RATE = 20
local EVENT_BURST = 40
local SIGNAL_QUEUE_MAX = 100
local SIGNAL_NAME_BYTES = 256
local SOURCE_MAX_BYTES = 1024 * 1024
local QUEUE_MAX_EVENTS = 2048
local QUEUE_MAX_BYTES = 8 * 1024 * 1024
local POLL_MAX_EVENTS = 64
local POLL_MAX_BYTES = 512 * 1024

local function finiteNumber(value, fallback)
	local number = tonumber(value)
	if number == nil or number ~= number or number == math.huge or number == -math.huge then
		return fallback
	end
	return number
end

local function tableCount(value)
	local count = 0
	for _ in pairs(value) do
		count += 1
	end
	return count
end

local function removeValue(items, target)
	for index, value in ipairs(items) do
		if value == target then
			table.remove(items, index)
			return true
		end
	end
	return false
end

local function safeValue(value, depth, state)
	depth = depth or 0
	state = state or { nodes = 0, seen = {} }
	state.nodes += 1
	if state.nodes > 20000 then
		return "<node-budget-exceeded>"
	end
	if depth > 16 then
		return "<max-depth>"
	end
	local kind = typeof(value)
	if kind == "nil" then
		return { __type = "Nil" }
	elseif kind == "boolean" then
		return value
	elseif kind == "number" then
		if value ~= value then
			return { __type = "Number", value = "NaN" }
		elseif value == math.huge then
			return { __type = "Number", value = "Infinity" }
		elseif value == -math.huge then
			return { __type = "Number", value = "-Infinity" }
		end
		return value
	elseif kind == "string" then
		if #value > 4 * 1024 * 1024 then
			return string.sub(value, 1, 4 * 1024 * 1024) .. "<truncated>"
		end
		return value
	elseif kind == "Instance" then
		local names = {}
		local cursor = value
		while cursor and cursor ~= game do
			table.insert(names, 1, cursor.Name)
			cursor = cursor.Parent
		end
		return { __type = "Instance", path = table.concat(names, "/"), class = value.ClassName }
	elseif kind == "EnumItem" then
		return { __type = "EnumItem", enum = tostring(value.EnumType), name = value.Name, value = value.Value }
	elseif kind == "Vector2" then
		return { __type = "Vector2", x = value.X, y = value.Y }
	elseif kind == "Vector3" then
		return { __type = "Vector3", x = value.X, y = value.Y, z = value.Z }
	elseif kind == "Color3" then
		return { __type = "Color3", r = value.R, g = value.G, b = value.B }
	elseif kind == "CFrame" then
		return { __type = "CFrame", components = { value:GetComponents() } }
	elseif kind ~= "table" then
		return tostring(value)
	end

	if state.seen[value] then
		return "<cycle>"
	end
	state.seen[value] = true
	local keys = {}
	local array = true
	local maximum = 0
	for key in pairs(value) do
		table.insert(keys, key)
		if #keys > 10000 then
			break
		end
		if type(key) ~= "number" or key < 1 or key ~= math.floor(key) then
			array = false
		else
			maximum = math.max(maximum, key)
		end
	end
	local truncated = #keys > 10000
	while #keys > 10000 do
		table.remove(keys)
	end
	array = array and not truncated and maximum == #keys
	local out
	if array then
		out = table.create(#keys)
		for index = 1, #keys do
			out[index] = safeValue(value[index], depth + 1, state)
		end
	else
		local stringKeys = true
		for _, key in ipairs(keys) do
			if type(key) ~= "string" then
				stringKeys = false
				break
			end
		end
		if stringKeys then
			out = {}
			table.sort(keys)
			for _, key in ipairs(keys) do
				out[key] = safeValue(value[key], depth + 1, state)
			end
			if truncated then
				out.__rosyncTruncated = true
			end
		else
			out = { __type = "Table", entries = {}, truncated = truncated }
			table.sort(keys, function(a, b)
				return tostring(a) < tostring(b)
			end)
			for _, key in ipairs(keys) do
				table.insert(out.entries, {
					key = safeValue(key, depth + 1, state),
					value = safeValue(value[key], depth + 1, state),
				})
			end
		end
	end
	state.seen[value] = nil
	return out
end

local function encodeValue(httpService, value)
	local encodedValue = safeValue(value)
	local ok, encoded = pcall(function()
		return httpService:JSONEncode(encodedValue)
	end)
	if not ok then
		return nil, nil, tostring(encoded)
	end
	return encodedValue, encoded, nil
end

local function resultValue(httpService, value)
	local encodedValue, encoded, failure = encodeValue(httpService, value)
	if encodedValue == nil or encoded == nil then
		return nil, failure
	end
	if #encoded > RESULT_BYTES then
		return { truncated = true, bytes = #encoded }, nil
	end
	return encodedValue, nil
end

local function frameAuth(sha256Hex, token, direction, runtimeId, sequence, bodyJson)
	return sha256Hex(
		token
			.. "|playscript-frame|"
			.. direction
			.. "|"
			.. runtimeId
			.. "|"
			.. tostring(sequence)
			.. "|"
			.. sha256Hex(bodyJson)
	)
end

local Runtime = {}
Runtime.__index = Runtime

function Playscript.newRuntime(options)
	local self = setmetatable({}, Runtime)
	self.httpService = options.httpService
	self.encodingService = options.encodingService
	self.scriptEditorService = options.scriptEditorService
	self.sha256Hex = options.sha256Hex
	self.runtimeId = options.runtimeId
	self.jobId = options.jobId
	self.sessionToken = options.sessionToken
	self.role = options.role
	self.connections = setmetatable({}, { __mode = "k" })
	self.activeConnection = nil
	self.inboundSequences = setmetatable({}, { __mode = "k" })
	self.outboundSequence = 0
	self.context = nil
	self.runners = {}
	self.resultSessions = {}
	self.resultOrder = {}
	self.resultCounter = 0
	self.clientCount = 0
	self.tokens = EVENT_BURST
	self.lastTokenClock = os.clock()
	self.dropped = 0
	self.cancelled = false
	self.heartbeatStarted = false
	self.pendingImportant = {}
	self.signalBacklog = {}
	self.signalSequence = 0
	return self
end

function Runtime:_connection()
	if self.activeConnection and self.activeConnection.Connected then
		return self.activeConnection
	end
	self.activeConnection = nil
	for connection in pairs(self.connections) do
		if connection.Connected then
			self.activeConnection = connection
			return connection
		end
	end
	return nil
end

function Runtime:_send(frameType, body)
	local connection = self:_connection()
	if not connection then
		return false
	end
	body.type = frameType
	local okBody, bodyJson = pcall(function()
		return self.httpService:JSONEncode(body)
	end)
	if not okBody then
		return false
	end
	self.outboundSequence += 1
	local sequence = self.outboundSequence
	local message = {
		kind = FRAME_KIND,
		playtestJobId = self.jobId,
		runtimeId = self.runtimeId,
		seq = sequence,
		bodyJson = bodyJson,
		auth = frameAuth(self.sha256Hex, self.sessionToken, "runtime-edit", self.runtimeId, sequence, bodyJson),
	}
	local okEncoded, encoded = pcall(function()
		return self.httpService:JSONEncode(message)
	end)
	if not okEncoded then
		return false
	end
	local sent = pcall(function()
		connection:SendMessage(encoded)
	end)
	return sent
end

function Runtime:_sendImportant(frameType, body)
	if self:_send(frameType, body) then
		return true
	end
	table.insert(self.pendingImportant, { frameType = frameType, body = body })
	while #self.pendingImportant > 32 do
		table.remove(self.pendingImportant, 1)
	end
	return false
end

function Runtime:_flushImportant()
	while #self.pendingImportant > 0 do
		local pending = self.pendingImportant[1]
		if not self:_send(pending.frameType, pending.body) then
			break
		end
		table.remove(self.pendingImportant, 1)
	end
end

function Runtime:_refillTokens()
	local now = os.clock()
	self.tokens = math.min(EVENT_BURST, self.tokens + (now - self.lastTokenClock) * EVENT_RATE)
	self.lastTokenClock = now
end

function Runtime:_takeToken()
	self:_refillTokens()
	if self.tokens < 1 then
		return false
	end
	self.tokens -= 1
	return true
end

function Runtime:_flushDropped()
	if self.dropped <= 0 then
		return
	end
	local count = self.dropped
	if self:_send("dropped", { context = self.context, count = count }) then
		self.dropped = 0
	end
end

function Runtime:_sendLimited(frameType, data)
	if not self:_takeToken() then
		self.dropped += 1
		return false
	end
	local encodedValue, encoded = encodeValue(self.httpService, data)
	if encodedValue == nil or encoded == nil or #encoded > EVENT_BYTES then
		self.dropped += 1
		return false
	end
	self:_flushDropped()
	local sent = self:_send(frameType, { context = self.context, data = encodedValue })
	if not sent then
		self.dropped += 1
	end
	return sent
end

function Runtime:attachConnection(connection)
	self.connections[connection] = true
	-- A reconnect may leave the old PluginConnection reporting Connected briefly;
	-- edit mode always binds the newest hello, so prefer the newest connection too.
	self.activeConnection = connection
	if self.heartbeatStarted then
		return
	end
	self.heartbeatStarted = true
	task.spawn(function()
		while not self.cancelled do
			task.wait(HEARTBEAT_SECONDS)
			self:_flushImportant()
			self:_flushDropped()
			self:_send("heartbeat", {
				context = self.context,
				uptime = os.clock(),
				clientCount = self.clientCount,
			})
			self:_cleanupResults()
		end
	end)
end

function Runtime:onStudioLog(level, message)
	for _, runner in pairs(self.runners) do
		if not runner.finished and not runner.destroyed then
			local threshold = runner.logs
			local include = threshold == "info"
				or (threshold == "warn" and (level == "warn" or level == "error"))
				or (threshold == "error" and level == "error")
			if include then
				local text = tostring(message)
				if #text > 16 * 1024 then
					text = string.sub(text, 1, 16 * 1024) .. "<truncated>"
				end
				self:_sendLimited("log", { level = level, message = text })
				break
			end
		end
	end
end

function Runtime:_cleanupResults()
	local now = os.clock()
	local kept = {}
	for _, resultId in ipairs(self.resultOrder) do
		local session = self.resultSessions[resultId]
		if session and now - session.createdAt < RESULT_TTL_SECONDS then
			table.insert(kept, resultId)
		else
			self.resultSessions[resultId] = nil
		end
	end
	self.resultOrder = kept
	while #self.resultOrder > RESULT_MAX_SESSIONS do
		local evicted = table.remove(self.resultOrder, 1)
		self.resultSessions[evicted] = nil
	end
end

function Runtime:_storeResult(envelope)
	local encoded = self.httpService:JSONEncode(envelope)
	if #encoded > RESULT_BYTES then
		local originalBytes = #encoded
		if envelope.ok == true then
			envelope = {
				ok = true,
				value = { truncated = true, bytes = originalBytes },
			}
		else
			envelope = {
				ok = false,
				error = "playscript failure envelope exceeded 1 MiB",
				truncated = true,
				bytes = originalBytes,
			}
		end
		encoded = self.httpService:JSONEncode(envelope)
	end
	self.resultCounter += 1
	local resultId = tostring(self.resultCounter) .. "-" .. self.httpService:GenerateGUID(false)
	self.resultSessions[resultId] = { bytes = encoded, createdAt = os.clock() }
	table.insert(self.resultOrder, resultId)
	self:_cleanupResults()
	return {
		id = resultId,
		bytes = #encoded,
		sha256 = self.sha256Hex(encoded),
	}
end

function Runtime:readResult(args)
	self:_cleanupResults()
	local resultId = tostring(args.id or args.resultId or "")
	local session = self.resultSessions[resultId]
	if not session then
		error("playscript result session not found: " .. resultId)
	end
	local offset = math.floor(finiteNumber(args.offset, 0))
	if offset < 0 or offset >= #session.bytes then
		error("playscript result offset is outside the result")
	end
	local maximum = math.clamp(math.floor(finiteNumber(args.maxBytes, RESULT_CHUNK_BYTES)), 1, RESULT_CHUNK_BYTES)
	local count = math.min(maximum, #session.bytes - offset)
	local raw = string.sub(session.bytes, offset + 1, offset + count)
	local encoded = self.encodingService:Base64Encode(buffer.fromstring(raw))
	if typeof(encoded) == "buffer" then
		encoded = buffer.tostring(encoded)
	end
	return {
		id = resultId,
		offset = offset,
		nextOffset = offset + count,
		eof = offset + count >= #session.bytes,
		bytesBase64 = tostring(encoded),
	}
end

function Runtime:closeResult(args)
	local resultId = tostring(args.id or args.resultId or "")
	self.resultSessions[resultId] = nil
	removeValue(self.resultOrder, resultId)
	return true
end

function Runtime:cancel(reason)
	self:_flushImportant()
	self:_flushDropped()
	self.cancelled = true
	for _, runner in pairs(self.runners) do
		self:_destroyRunner(runner)
	end
	return { cancelled = true, reason = tostring(reason or "cancelled") }
end

function Runtime:registerHandlers(handlers)
	handlers.playscript_result_read = function(args)
		return self:readResult(args)
	end
	handlers.playscript_result_close = function(args)
		return self:closeResult(args)
	end
	handlers.playscript_cancel = function(args)
		return self:cancel(args.reason)
	end
end

local function runnerParent(identity, role)
	if identity == "plugin" then
		return script, "Script"
	elseif role == "server" then
		return game:GetService("ServerScriptService"), "Script"
	end
	local player = game:GetService("Players").LocalPlayer
	if not player then
		return nil, nil
	end
	return player:FindFirstChildOfClass("PlayerScripts") or player:WaitForChild("PlayerScripts", 5), "LocalScript"
end

function Runtime:_destroyRunner(runner)
	if runner.destroyed then
		return
	end
	runner.destroyed = true
	local signalWaiters = runner.signalWaiters
	runner.signalWaiters = {}
	for _, waiters in pairs(signalWaiters) do
		local claimed = table.clone(waiters)
		table.clear(waiters)
		for _, waiter in ipairs(claimed) do
			waiter:Fire(false)
		end
	end
	local clientWaiters = runner.clientWaiters
	runner.clientWaiters = {}
	for _, waiter in ipairs(clientWaiters) do
		waiter.event:Fire(false)
	end
	runner.signalQueues = {}
	runner.queuedSignalCount = 0
	pcall(function()
		runner.script.Disabled = true
	end)
	pcall(function()
		runner.holder:Destroy()
	end)
end

function Runtime:_finishRunner(runner, ok, value, traceback, explicitCompletion)
	if runner.finished then
		return false
	end
	runner.finished = true
	self:_flushDropped()
	local envelope
	if ok then
		local encoded, failure = resultValue(self.httpService, value)
		if failure then
			envelope = { ok = false, error = "result encoding failed: " .. failure }
		else
			envelope = { ok = true, value = encoded }
		end
	else
		envelope = {
			ok = false,
			error = string.sub(tostring(value), 1, 64 * 1024),
			traceback = string.sub(tostring(traceback or value), 1, 256 * 1024),
		}
	end
	local resultRef = self:_storeResult(envelope)
	local frameType = if runner.kind == "main" or explicitCompletion == true then "complete" else "clientResult"
	self:_sendImportant(frameType, {
		context = self.context,
		runner = runner.kind,
		ok = envelope.ok,
		result = resultRef,
	})
	task.defer(function()
		self:_destroyRunner(runner)
	end)
	return true
end

function Runtime:_queueSignal(runner, name, payload)
	if runner.queuedSignalCount >= SIGNAL_QUEUE_MAX then
		self.dropped += 1
		return false
	end
	local queue = runner.signalQueues[name]
	if not queue then
		queue = {}
		runner.signalQueues[name] = queue
	end
	table.insert(queue, payload)
	runner.queuedSignalCount += 1
	return true
end

function Runtime:_awaitSignal(runner, name, timeoutSeconds)
	name = tostring(name or "")
	if name == "" then
		error("playtest.await name must not be empty")
	elseif #name > SIGNAL_NAME_BYTES then
		error("playtest.await name exceeds 256 bytes")
	end
	local queue = runner.signalQueues[name]
	if queue and #queue > 0 then
		local payload = table.remove(queue, 1)
		runner.queuedSignalCount = math.max(0, runner.queuedSignalCount - 1)
		if #queue == 0 then
			runner.signalQueues[name] = nil
		end
		return payload
	end
	local timeout = math.clamp(finiteNumber(timeoutSeconds, 30), 0, 3600)
	if timeout <= 0 then
		return nil
	end
	local waiter = Instance.new("BindableEvent")
	local waiters = runner.signalWaiters[name]
	if not waiters then
		waiters = {}
		runner.signalWaiters[name] = waiters
	end
	table.insert(waiters, waiter)
	task.delay(timeout, function()
		local index = table.find(waiters, waiter)
		if index == nil then
			return
		end
		table.remove(waiters, index)
		if #waiters == 0 and runner.signalWaiters[name] == waiters then
			runner.signalWaiters[name] = nil
		end
		waiter:Fire(false)
	end)
	local received, payload = waiter.Event:Wait()
	removeValue(waiters, waiter)
	if #waiters == 0 and runner.signalWaiters[name] == waiters then
		runner.signalWaiters[name] = nil
	end
	waiter:Destroy()
	if received then
		return payload
	end
	return nil
end

function Runtime:_awaitClients(runner, minimum, timeoutSeconds)
	if self.role ~= "server" then
		error("playtest.awaitClients is only available in the server context")
	end
	local needed = math.clamp(math.floor(finiteNumber(minimum, 1)), 1, 8)
	if self.clientCount >= needed then
		return self.clientCount
	end
	local timeout = math.clamp(finiteNumber(timeoutSeconds, 30), 0, 3600)
	if timeout <= 0 then
		return nil
	end
	local event = Instance.new("BindableEvent")
	local waiter = { minimum = needed, event = event }
	table.insert(runner.clientWaiters, waiter)
	task.delay(timeout, function()
		if not removeValue(runner.clientWaiters, waiter) then
			return
		end
		event:Fire(false)
	end)
	local ready, count = event.Event:Wait()
	removeValue(runner.clientWaiters, waiter)
	event:Destroy()
	if ready then
		return count
	end
	return nil
end

function Runtime:_bridgeCall(runner, action, ...)
	if (runner.finished or runner.destroyed) and action ~= "__return" then
		return nil
	end
	local arguments = table.pack(...)
	if action == "emit" then
		return self:_sendLimited("event", arguments[1])
	elseif action == "log" then
		return self:_sendLimited("event", { log = tostring(arguments[1]) })
	elseif action == "signal" then
		local name = tostring(arguments[1] or "")
		if name == "" then
			error("playtest.signal name must not be empty")
		elseif #name > SIGNAL_NAME_BYTES then
			error("playtest.signal name exceeds 256 bytes")
		end
		if not self:_takeToken() then
			self.dropped += 1
			return false
		end
		local encodedValue, encoded = encodeValue(self.httpService, arguments[2])
		if encodedValue == nil or encoded == nil or #encoded > EVENT_BYTES then
			self.dropped += 1
			return false
		end
		self:_flushDropped()
		local sent = self:_send("signal", { context = self.context, name = name, payload = encodedValue })
		if not sent then
			self.dropped += 1
		end
		return sent
	elseif action == "await" then
		return self:_awaitSignal(runner, arguments[1], arguments[2])
	elseif action == "awaitClients" then
		return self:_awaitClients(runner, arguments[1], arguments[2])
	elseif action == "done" then
		self:_finishRunner(runner, true, arguments[1], nil, true)
		return true
	elseif action == "fail" then
		self:_finishRunner(runner, false, arguments[1], arguments[2], true)
		return true
	elseif action == "__return" then
		local packed = arguments[1]
		if type(packed) ~= "table" then
			return self:_finishRunner(runner, false, "playscript returned an invalid result envelope")
		end
		if packed[1] == true then
			return self:_finishRunner(runner, true, packed[2])
		end
		local failure = packed[2]
		if type(failure) == "table" then
			return self:_finishRunner(
				runner,
				false,
				tostring(failure.message or "playscript failed"),
				tostring(failure.traceback or failure.message or "playscript failed")
			)
		end
		return self:_finishRunner(runner, false, failure, failure)
	end
	error("unknown playscript bridge action: " .. tostring(action))
end

function Runtime:_deliverSignal(name, payload)
	if name == "" or #name > SIGNAL_NAME_BYTES then
		self.dropped += 1
		return
	end
	self.signalSequence += 1
	table.insert(self.signalBacklog, {
		name = name,
		payload = payload,
		seq = self.signalSequence,
		createdAt = os.clock(),
	})
	while #self.signalBacklog > 100 do
		table.remove(self.signalBacklog, 1)
	end
	for _, runner in pairs(self.runners) do
		if not runner.finished and not runner.destroyed then
			runner.lastSignalSequence = self.signalSequence
			local waiters = runner.signalWaiters[name]
			if waiters and #waiters > 0 then
				local claimed = table.clone(waiters)
				table.clear(waiters)
				if runner.signalWaiters[name] == waiters then
					runner.signalWaiters[name] = nil
				end
				for _, waiter in ipairs(claimed) do
					waiter:Fire(true, payload)
				end
			else
				self:_queueSignal(runner, name, payload)
			end
		end
	end
end

function Runtime:_updateClients(count)
	self.clientCount = math.clamp(math.floor(finiteNumber(count, 0)), 0, 8)
	for _, runner in pairs(self.runners) do
		if not runner.finished and not runner.destroyed then
			for _, waiter in ipairs(table.clone(runner.clientWaiters)) do
				if self.clientCount >= waiter.minimum then
					if removeValue(runner.clientWaiters, waiter) then
						waiter.event:Fire(true, self.clientCount)
					end
				end
			end
		end
	end
end

function Runtime:_boot(body)
	if self.cancelled then
		return
	end
	local kind = tostring(body.runner or body.runnerKind or "")
	if kind ~= "main" and kind ~= "client" then
		return
	end
	local existingRunner = self.runners[kind]
	if existingRunner then
		if existingRunner.installState == "installed" then
			self:_sendImportant("booted", {
				context = self.context,
				runner = kind,
				identity = existingRunner.identity,
			})
		elseif existingRunner.installState == "failed" then
			self:_sendImportant("bootFailure", {
				context = self.context,
				runner = kind,
				error = existingRunner.bootError or "playscript installation failed",
			})
		end
		return
	end
	local source = tostring(body.source or "")
	if source == "" or #source > SOURCE_MAX_BYTES then
		self:_sendImportant("bootFailure", {
			context = tostring(body.context or self.context or ""),
			runner = kind,
			error = if source == "" then "playscript source is empty" else "playscript source exceeds 1 MiB",
		})
		return
	end
	self.context = tostring(body.context or self.context or "")
	local identity = tostring(body.identity or "game"):lower()
	if identity ~= "game" and identity ~= "plugin" then
		identity = "game"
	end
	-- Reserve the runner before runnerParent can yield waiting for client
	-- PlayerScripts. Coordinator boot frames are retried, and an in-flight
	-- sentinel prevents those retries from installing the same source twice.
	local runner = {
		kind = kind,
		identity = identity,
		installState = "installing",
		logs = tostring(body.logs or "off"):lower(),
		holder = nil,
		script = nil,
		bridge = nil,
		finished = false,
		destroyed = false,
		signalQueues = {},
		queuedSignalCount = 0,
		signalWaiters = {},
		clientWaiters = {},
		lastSignalSequence = 0,
	}
	self.runners[kind] = runner
	local parent, className = runnerParent(identity, self.role)
	if self.cancelled or runner.destroyed or self.runners[kind] ~= runner then
		return
	end
	if not parent then
		runner.installState = "failed"
		runner.bootError = "playscript parent is unavailable"
		self:_sendImportant("bootFailure", {
			context = self.context,
			runner = kind,
			error = runner.bootError,
		})
		return
	end

	local holder = Instance.new("Folder")
	holder.Name = "__RoSyncPlayscript_" .. self.runtimeId .. "_" .. kind
	local bridge = Instance.new("BindableFunction")
	bridge.Name = "Bridge"
	bridge.Parent = holder
	local configValue = Instance.new("StringValue")
	configValue.Name = "Config"
	configValue.Value = self.httpService:JSONEncode({
		argsJson = tostring(body.argsJson or "{}"),
		mode = tostring(body.mode or "play"),
		context = self.context,
		jobId = self.jobId,
	})
	configValue.Parent = holder
	local runnerScript = Instance.new(className)
	local anyRunner = runnerScript
	runnerScript.Name = "Runner"
	anyRunner.Disabled = true
	if identity == "plugin" then
		local contextOk, contextError = pcall(function()
			anyRunner.RunContext = Enum.RunContext.Plugin
		end)
		if not contextOk then
			holder:Destroy()
			runner.destroyed = true
			runner.installState = "failed"
			runner.bootError = "plugin identity is unavailable: " .. tostring(contextError)
			self:_sendImportant("bootFailure", {
				context = self.context,
				runner = kind,
				error = runner.bootError,
			})
			return
		end
	end

	runner.holder = holder
	runner.script = runnerScript
	runner.bridge = bridge
	local backlogCutoff = os.clock() - 30
	for _, signal in ipairs(self.signalBacklog) do
		if signal.createdAt >= backlogCutoff and signal.seq > runner.lastSignalSequence then
			self:_queueSignal(runner, signal.name, signal.payload)
			runner.lastSignalSequence = signal.seq
		end
	end
	bridge.OnInvoke = function(action, ...)
		return self:_bridgeCall(runner, tostring(action), ...)
	end
	local wrapper = table.concat({
		'local HttpService = game:GetService("HttpService")',
		'local __config = HttpService:JSONDecode(script.Parent:WaitForChild("Config").Value)',
		'local __bridge = script.Parent:WaitForChild("Bridge")',
		"local playtest = { args = HttpService:JSONDecode(__config.argsJson), mode = __config.mode, context = __config.context, jobId = __config.jobId }",
		'function playtest.emit(data) return __bridge:Invoke("emit", data) end',
		'function playtest.log(message) return __bridge:Invoke("log", tostring(message)) end',
		'function playtest.done(value) return __bridge:Invoke("done", value) end',
		'function playtest.fail(message) return __bridge:Invoke("fail", tostring(message), debug.traceback(tostring(message), 2)) end',
		'function playtest.signal(name, payload) return __bridge:Invoke("signal", name, payload) end',
		'function playtest.await(name, timeoutSeconds) return __bridge:Invoke("await", name, timeoutSeconds) end',
		'function playtest.awaitClients(count, timeoutSeconds) return __bridge:Invoke("awaitClients", count, timeoutSeconds) end',
		"local function __rosync_run()",
		source,
		"end",
		"local __packed = table.pack(xpcall(__rosync_run, function(errorValue) return { message = tostring(errorValue), traceback = debug.traceback(tostring(errorValue), 2) } end))",
		'__bridge:Invoke("__return", __packed)',
	}, "\n")

	local runnerNeedle = holder.Name .. ".Runner"
	local compileConnection
	local scriptErrorConnection
	compileConnection = game:GetService("LogService").MessageOut:Connect(function(message, messageType)
		if messageType == Enum.MessageType.MessageError and string.find(tostring(message), runnerNeedle, 1, true) then
			self:_finishRunner(runner, false, "compile: " .. tostring(message), tostring(message))
		end
	end)
	scriptErrorConnection = game:GetService("ScriptContext").Error:Connect(function(message, stack, errorSource)
		if
			errorSource == runnerScript
			or string.find(tostring(message), holder.Name, 1, true)
			or string.find(tostring(stack), holder.Name, 1, true)
		then
			self:_finishRunner(runner, false, tostring(message), tostring(stack))
		end
	end)
	local sourceOk, sourceError = pcall(function()
		anyRunner.Source = wrapper
	end)
	if not sourceOk and self.scriptEditorService then
		holder.Parent = parent
		runnerScript.Parent = holder
		sourceOk, sourceError = pcall(function()
			self.scriptEditorService:UpdateSourceAsync(runnerScript, function()
				return wrapper
			end)
		end)
	end
	if self.cancelled or runner.destroyed or self.runners[kind] ~= runner then
		compileConnection:Disconnect()
		scriptErrorConnection:Disconnect()
		pcall(function()
			holder:Destroy()
		end)
		return
	end
	if not sourceOk then
		compileConnection:Disconnect()
		scriptErrorConnection:Disconnect()
		holder:Destroy()
		runner.destroyed = true
		runner.installState = "failed"
		runner.bootError = "unable to install playscript: " .. tostring(sourceError)
		self:_sendImportant("bootFailure", {
			context = self.context,
			runner = kind,
			error = runner.bootError,
		})
		return
	end
	if holder.Parent == nil then
		holder.Parent = parent
	end
	if runnerScript.Parent == nil then
		runnerScript.Parent = holder
	end
	anyRunner.Disabled = false
	runner.installState = "installed"
	self:_sendImportant("booted", { context = self.context, runner = kind, identity = identity })
	task.spawn(function()
		while not runner.finished and not runner.destroyed do
			task.wait(0.25)
		end
		compileConnection:Disconnect()
		scriptErrorConnection:Disconnect()
	end)
end

function Runtime:handleFrame(connection, message)
	if tostring(message.playtestJobId or "") ~= self.jobId or tostring(message.runtimeId or "") ~= self.runtimeId then
		return false
	end
	local sequence = math.floor(finiteNumber(message.seq, -1))
	local previous = self.inboundSequences[connection] or 0
	if sequence <= previous then
		return false
	end
	local bodyJson = tostring(message.bodyJson or "")
	if bodyJson == "" or #bodyJson > 3 * 1024 * 1024 then
		return false
	end
	local expected = frameAuth(self.sha256Hex, self.sessionToken, "edit-runtime", self.runtimeId, sequence, bodyJson)
	if tostring(message.auth or "") ~= expected then
		return false
	end
	local ok, body = pcall(function()
		return self.httpService:JSONDecode(bodyJson)
	end)
	if not ok or type(body) ~= "table" then
		return false
	end
	self.inboundSequences[connection] = sequence
	local frameType = tostring(body.type or "")
	if frameType == "boot" then
		self:_boot(body)
	elseif frameType == "signal" then
		self:_deliverSignal(tostring(body.name or ""), body.payload)
	elseif frameType == "clients" then
		self:_updateClients(body.count)
	elseif frameType == "cancel" then
		self:cancel(body.reason)
	end
	return true
end

local Coordinator = {}
Coordinator.__index = Coordinator

function Playscript.newCoordinator(options)
	local self = setmetatable({}, Coordinator)
	self.httpService = options.httpService
	self.sha256Hex = options.sha256Hex
	self.onTerminalRequested = options.onTerminalRequested
	return self
end

function Coordinator:_snapshot(job)
	local run = job and job.run
	if not run then
		return nil
	end
	return {
		status = run.status,
		outcome = run.terminal,
		terminal = run.terminal,
		keepOpen = run.keepOpen,
		context = run.mainContext,
		identity = run.identity,
		mode = job.mode,
		timeout = run.timeout,
		createdAt = run.createdAt,
		startedAt = run.startedAt,
		lastSeq = run._nextSequence - 1,
		jobId = job.id,
		clientRunId = job.clientRunId,
	}
end

function Coordinator:snapshot(job)
	return self:_snapshot(job)
end

function Coordinator:_eventBytes(event)
	local ok, encoded = pcall(function()
		return self.httpService:JSONEncode(event)
	end)
	return if ok then #encoded else 1024
end

function Coordinator:_enqueueDirect(job, event)
	local run = job.run
	event.seq = run._nextSequence
	run._nextSequence += 1
	event.t = event.t or math.max(0, os.clock() - run._startedClock)
	local bytes = self:_eventBytes(event)
	event.__bytes = bytes
	table.insert(run._events, event)
	run._eventBytes += bytes
end

function Coordinator:_makeQueueRoom(job, requiredBytes)
	local run = job.run
	while #run._events >= QUEUE_MAX_EVENTS or run._eventBytes + requiredBytes > QUEUE_MAX_BYTES do
		local removed = table.remove(run._events, 1)
		if not removed then
			break
		end
		run._eventBytes -= removed.__bytes or 0
		run._queueDropped += if removed.type == "dropped" then math.max(1, tonumber(removed.count) or 1) else 1
	end
end

function Coordinator:_flushQueueDropped(job)
	local run = job.run
	if run._queueDropped <= 0 then
		return
	end
	local marker = { type = "dropped", context = "coordinator", count = 0 }
	while true do
		marker.count = run._queueDropped
		local before = run._queueDropped
		self:_makeQueueRoom(job, self:_eventBytes(marker))
		if run._queueDropped == before then
			break
		end
	end
	marker.count = run._queueDropped
	run._queueDropped = 0
	self:_enqueueDirect(job, marker)
end

function Coordinator:_enqueue(job, event)
	local run = job.run
	if not run then
		return
	end
	self:_flushQueueDropped(job)
	local bytes = self:_eventBytes(event)
	if #run._events >= QUEUE_MAX_EVENTS or run._eventBytes + bytes > QUEUE_MAX_BYTES then
		run._queueDropped += if event.type == "dropped" then math.max(1, tonumber(event.count) or 1) else 1
		return
	end
	self:_enqueueDirect(job, event)
end

function Coordinator:_send(context, frameType, body)
	if not context or not context.connection.Connected then
		return false
	end
	context.playscriptOutboundSequence = (context.playscriptOutboundSequence or 0) + 1
	local sequence = context.playscriptOutboundSequence
	body.type = frameType
	local bodyJson = self.httpService:JSONEncode(body)
	local message = {
		kind = FRAME_KIND,
		playtestJobId = context.playtestJobId,
		runtimeId = context.runtimeId,
		seq = sequence,
		bodyJson = bodyJson,
		auth = frameAuth(self.sha256Hex, context.sessionToken, "edit-runtime", context.runtimeId, sequence, bodyJson),
	}
	local encoded = self.httpService:JSONEncode(message)
	local sent = pcall(function()
		context.connection:SendMessage(encoded)
	end)
	return sent
end

function Coordinator:createRun(job, config)
	if type(config) ~= "table" then
		error("playtest run configuration must be an object")
	end
	local main = if type(config.script) == "table" then config.script else {}
	local source = tostring(main.source or config.source or "")
	if source == "" then
		error("playtest run requires script.source")
	end
	if #source > SOURCE_MAX_BYTES then
		error("playtest run script exceeds 1 MiB")
	end
	local companion = if type(config.clientScript) == "table" then config.clientScript else nil
	if companion and #tostring(companion.source or "") > SOURCE_MAX_BYTES then
		error("playtest run client script exceeds 1 MiB")
	end
	local mainContext = tostring(config.context or "server")
	if mainContext ~= "server" and not string.match(mainContext, "^client:%d+$") then
		error("playtest run context must be server or client:N")
	end
	if job.mode == "run" and mainContext ~= "server" then
		error("Studio run mode has no client contexts")
	end
	if job.mode == "run" and companion then
		error("Studio run mode cannot execute a client script")
	end
	local identity = tostring(config.identity or "game"):lower()
	if identity ~= "game" and identity ~= "plugin" then
		error("playtest run identity must be game or plugin")
	end
	local timeout = math.clamp(finiteNumber(config.timeout, 600), 0.1, 3600)
	local argsJson = tostring(config.scriptArgsJson or "")
	if argsJson == "" then
		local encodeOk, encodedArgs = pcall(function()
			return self.httpService:JSONEncode(if config.scriptArgs == nil then {} else config.scriptArgs)
		end)
		if not encodeOk then
			error("playtest run args could not be encoded: " .. tostring(encodedArgs))
		end
		argsJson = encodedArgs
	end
	local argsOk = pcall(function()
		self.httpService:JSONDecode(argsJson)
	end)
	if not argsOk then
		error("playtest run scriptArgsJson is not valid JSON")
	end
	local now = os.clock()
	job.run = {
		status = "booting",
		terminal = nil,
		_pendingTerminal = nil,
		keepOpen = config.keepOpen == true,
		mainContext = mainContext,
		identity = identity,
		timeout = timeout,
		createdAt = os.time(),
		startedAt = os.time(),
		_startedClock = now,
		_main = { source = source, path = tostring(main.path or ""), sha256 = self.sha256Hex(source) },
		_client = if companion and tostring(companion.source or "") ~= ""
			then {
				source = tostring(companion.source),
				path = tostring(companion.path or ""),
				sha256 = self.sha256Hex(tostring(companion.source)),
			}
			else nil,
		_argsJson = argsJson,
		_logs = tostring(config.logs or "off"):lower(),
		_events = {},
		_eventBytes = 0,
		_queueDropped = 0,
		_nextSequence = 1,
		_heartbeats = {},
		_readyRuntimes = {},
		_booted = {},
		_bootSentAt = {},
		_bootRetryScheduled = {},
		_pendingResults = {},
	}
	task.delay(math.min(45, timeout), function()
		local run = job.run
		if
			run
			and not run.terminal
			and not run._pendingTerminal
			and not run._completionClaim
			and not run._booted.main
		then
			self:requestTerminal(job, {
				outcome = "bootFailure",
				kind = "bootFailure",
				error = "required playscript context did not become ready",
				jobStatus = job.status,
			})
		end
	end)
	task.delay(timeout, function()
		local run = job.run
		if run and not run.terminal and not run._pendingTerminal and not run._completionClaim then
			self:requestTerminal(job, {
				outcome = "timeout",
				kind = "timeout",
				error = "playtest run timed out after " .. tostring(timeout) .. "s",
				jobStatus = job.status,
			})
		end
	end)
	task.spawn(function()
		while job.run and not job.run.terminal and not job.run._pendingTerminal and not job.run._completionClaim do
			task.wait(2)
			local run = job.run
			if run and run._booted.main then
				local context = run._contexts and run._contexts[run.mainContext]
				local heartbeat = run._heartbeats[run.mainContext]
				local reference = heartbeat or run._mainBootClock or run._startedClock
				local connected = context ~= nil and context.connection ~= nil and context.connection.Connected
				local heartbeatAge = math.max(0, os.clock() - reference)
				if not connected then
					run._heartbeatStale = {
						context = run.mainContext,
						ageSeconds = heartbeatAge,
						jobStatus = job.status,
						disconnected = true,
					}
					self:requestTerminal(job, {
						outcome = "aborted",
						kind = "aborted",
						error = "playscript runtime disconnected",
						jobStatus = job.status,
					})
				elseif heartbeatAge > 6 then
					run._heartbeatStale = {
						context = run.mainContext,
						ageSeconds = heartbeatAge,
						jobStatus = job.status,
						disconnected = false,
					}
					local active = job.status == "starting" or job.status == "running" or job.status == "stopping"
					if not active then
						self:requestTerminal(job, {
							outcome = "aborted",
							kind = "aborted",
							error = "playscript runtime heartbeat stopped",
							jobStatus = job.status,
						})
					end
				else
					run._heartbeatStale = nil
				end
			end
		end
	end)
	return self:_snapshot(job)
end

function Coordinator:auditStart(job)
	local run = job.run
	return {
		subcommand = "playtest run",
		jobId = job.id,
		clientRunId = job.clientRunId,
		mode = job.mode,
		players = job.players,
		context = run.mainContext,
		identity = run.identity,
		timeout = run.timeout,
		keepOpen = run.keepOpen,
		argsJson = run._argsJson,
		script = { path = run._main.path, sha256 = run._main.sha256, bytes = #run._main.source },
		clientScript = if run._client
			then { path = run._client.path, sha256 = run._client.sha256, bytes = #run._client.source }
			else nil,
	}
end

function Coordinator:_bootContext(job, context, runnerKind, scriptConfig)
	local run = job.run
	local key = context.runtimeId .. "|" .. runnerKind
	if run._booted[key] == "ready" or run._booted[key] == "failed" then
		return
	end
	local function scheduleRetry(delaySeconds)
		if run._bootRetryScheduled[key] then
			return
		end
		run._bootRetryScheduled[key] = true
		task.delay(math.max(0.05, delaySeconds), function()
			if job.run ~= run then
				return
			end
			run._bootRetryScheduled[key] = nil
			local state = run._booted[key]
			if
				not run.terminal
				and not run._pendingTerminal
				and state ~= "ready"
				and state ~= "failed"
				and context.connection.Connected
			then
				self:_bootContext(job, context, runnerKind, scriptConfig)
			end
		end)
	end
	local now = os.clock()
	local sinceLastSend = now - (run._bootSentAt[key] or 0)
	if run._booted[key] == "sent" and sinceLastSend < 1 then
		scheduleRetry(1 - sinceLastSend)
		return
	end
	local sent = self:_send(context, "boot", {
		runner = runnerKind,
		context = context.name,
		mode = job.mode,
		identity = run.identity,
		source = scriptConfig.source,
		argsJson = run._argsJson,
		logs = run._logs,
		jobId = job.id,
	})
	if sent then
		run._booted[key] = "sent"
		run._bootSentAt[key] = now
	end
	-- SendMessage may fail transiently while a freshly-created runtime context
	-- is still settling. Keep trying until the authenticated boot ack arrives.
	scheduleRetry(1)
end

function Coordinator:onContext(job, context, allContexts)
	local run = job and job.run
	if not run or run.terminal or run._pendingTerminal then
		return
	end
	if not run._readyRuntimes[context.runtimeId] then
		run._readyRuntimes[context.runtimeId] = true
		self:_enqueue(job, { type = "ready", context = context.name })
	end
	if context.name == run.mainContext then
		self:_bootContext(job, context, "main", run._main)
	end
	if context.role == "client" and run._client then
		self:_bootContext(job, context, "client", run._client)
	end
	self:onContextsChanged(job, allContexts)
end

function Coordinator:onContextsChanged(job, contexts)
	local run = job and job.run
	if not run or run.terminal or run._pendingTerminal then
		return
	end
	local clients = 0
	for _, context in pairs(contexts or {}) do
		if context.playtestJobId == job.id and context.connection.Connected and context.role == "client" then
			clients += 1
		end
	end
	for _, context in pairs(contexts or {}) do
		if context.playtestJobId == job.id and context.connection.Connected then
			self:_send(context, "clients", { count = clients })
		end
	end
end

function Coordinator:handleFrame(job, context, message)
	local run = job and job.run
	if not run or run.terminal or run._pendingTerminal then
		return nil
	end
	if tostring(message.playtestJobId or "") ~= job.id or tostring(message.runtimeId or "") ~= context.runtimeId then
		return nil
	end
	local sequence = math.floor(finiteNumber(message.seq, -1))
	local previous = context.playscriptInboundSequence or 0
	if sequence <= previous then
		return nil
	end
	local bodyJson = tostring(message.bodyJson or "")
	if bodyJson == "" or #bodyJson > 256 * 1024 then
		return nil
	end
	local expected =
		frameAuth(self.sha256Hex, context.sessionToken, "runtime-edit", context.runtimeId, sequence, bodyJson)
	if tostring(message.auth or "") ~= expected then
		return nil
	end
	local ok, body = pcall(function()
		return self.httpService:JSONDecode(bodyJson)
	end)
	if not ok or type(body) ~= "table" then
		return nil
	end
	context.playscriptInboundSequence = sequence
	local frameType = tostring(body.type or "")
	if frameType == "heartbeat" then
		run._heartbeats[context.name] = os.clock()
	elseif frameType == "booted" then
		local runner = tostring(body.runner or "")
		run._booted[context.runtimeId .. "|" .. runner] = "ready"
		if runner == "main" then
			run._booted.main = true
			run._mainBootClock = os.clock()
			run.status = "running"
		end
	elseif frameType == "event" then
		self:_enqueue(job, { type = "event", context = context.name, data = body.data })
	elseif frameType == "log" then
		local data = if type(body.data) == "table" then body.data else {}
		self:_enqueue(job, {
			type = "log",
			context = context.name,
			level = tostring(data.level or "info"),
			message = tostring(data.message or ""),
		})
	elseif frameType == "dropped" then
		self:_enqueue(job, {
			type = "dropped",
			context = context.name,
			count = math.max(1, math.floor(finiteNumber(body.count, 1))),
		})
	elseif frameType == "signal" then
		local name = tostring(body.name or "")
		if name ~= "" and #name <= SIGNAL_NAME_BYTES then
			for _, target in pairs(run._contexts or {}) do
				if target.playtestJobId == job.id and target.connection.Connected then
					self:_send(target, "signal", { name = name, payload = body.payload, origin = context.name })
				end
			end
		end
	elseif frameType == "bootFailure" then
		local runner = tostring(body.runner or "")
		local bootKey = context.runtimeId .. "|" .. runner
		if run._booted[bootKey] == "failed" then
			return nil
		end
		run._booted[bootKey] = "failed"
		if runner == "main" then
			self:requestTerminal(job, {
				outcome = "bootFailure",
				kind = "bootFailure",
				error = tostring(body.error or "playscript failed to boot"),
				context = context.name,
				jobStatus = job.status,
			})
		else
			self:_enqueue(job, {
				type = "clientResult",
				context = context.name,
				ok = false,
				error = tostring(body.error or "client playscript failed to boot"),
			})
		end
	elseif frameType == "complete" or frameType == "clientResult" then
		local result = if type(body.result) == "table" then body.result else nil
		if not result then
			return nil
		end
		local resultKey = context.runtimeId .. "|" .. tostring(result.id or "")
		if frameType == "complete" then
			if run._completionClaim then
				return nil
			end
			run._completionClaim = resultKey
		end
		if run._pendingResults[resultKey] then
			return nil
		end
		run._pendingResults[resultKey] = true
		return { job = job, context = context, body = body, result = result, key = resultKey }
	end
	return nil
end

function Coordinator:resolveResult(action, envelope, failure)
	local job = action.job
	local run = job and job.run
	if not run then
		return
	end
	run._pendingResults[action.key] = nil
	local body = action.body
	local clientResult = tostring(body.type or "") == "clientResult"
	if failure then
		if clientResult then
			self:_enqueue(job, {
				type = "clientResult",
				context = action.context.name,
				ok = false,
				error = "unable to read client result: " .. tostring(failure),
			})
		else
			run._completionClaim = nil
			self:requestTerminal(job, {
				outcome = "failure",
				kind = "error",
				error = "unable to read playscript result: " .. tostring(failure),
				context = action.context.name,
				jobStatus = job.status,
			})
		end
		return
	end
	if type(envelope) ~= "table" then
		return self:resolveResult(action, nil, "invalid result envelope")
	end
	if clientResult then
		self:_enqueue(job, {
			type = "clientResult",
			context = action.context.name,
			ok = envelope.ok == true,
			value = envelope.value,
			error = envelope.error,
			traceback = envelope.traceback,
		})
	elseif envelope.ok == true then
		run._completionClaim = nil
		self:requestTerminal(job, {
			outcome = "success",
			kind = "result",
			ok = true,
			value = envelope.value,
			context = action.context.name,
			jobStatus = job.status,
		})
	else
		run._completionClaim = nil
		self:requestTerminal(job, {
			outcome = "failure",
			kind = "error",
			ok = false,
			error = tostring(envelope.error or "playscript failed"),
			traceback = envelope.traceback,
			context = action.context.name,
			jobStatus = job.status,
		})
	end
end

function Coordinator:requestTerminal(job, terminal)
	local run = job and job.run
	if not run or run.terminal or run._pendingTerminal or run._completionClaim then
		return false
	end
	terminal.elapsed = terminal.elapsed or math.max(0, os.clock() - run._startedClock)
	terminal.jobStatus = terminal.jobStatus or job.status
	run._pendingTerminal = terminal
	run.status = "stopping"
	if self.onTerminalRequested then
		task.spawn(self.onTerminalRequested, job, terminal)
	end
	return true
end

function Coordinator:finalizeTerminal(job, terminal, stopError)
	local run = job and job.run
	if not run or run.terminal then
		return false
	end
	terminal = terminal or run._pendingTerminal
	if not terminal then
		return false
	end
	terminal.jobStatus = job.status
	if stopError then
		terminal.stopError = tostring(stopError)
	end
	run._pendingTerminal = nil
	run.terminal = terminal
	run.status = terminal.outcome
	self:_flushQueueDropped(job)
	local event = {
		type = if terminal.outcome == "aborted" then "aborted" else "result",
		ok = terminal.outcome == "success",
		outcome = terminal.outcome,
		elapsed = terminal.elapsed,
	}
	if terminal.outcome == "aborted" then
		event.reason = terminal.error or terminal.reason or "job ended externally"
		event.jobStatus = terminal.jobStatus
	end
	self:_enqueue(job, event)
	return true
end

function Coordinator:onJobEnded(job)
	local run = job and job.run
	if not run or run.terminal or run._pendingTerminal or run._completionClaim then
		return
	end
	local bootFailure = not run._booted.main and tostring(job.status) == "failed"
	local terminal = {
		outcome = if bootFailure then "bootFailure" else "aborted",
		kind = if bootFailure then "bootFailure" else "aborted",
		error = if bootFailure
			then "playtest ended before the main playscript booted"
			else "playtest job ended externally",
		jobStatus = job.status,
		elapsed = math.max(0, os.clock() - run._startedClock),
	}
	run._pendingTerminal = terminal
	run.status = "stopping"
	if self.onTerminalRequested then
		task.spawn(self.onTerminalRequested, job, terminal, true)
	else
		self:finalizeTerminal(job, terminal)
	end
end

function Coordinator:poll(job, args)
	local run = job and job.run
	if not run then
		error("playtest run not found")
	end
	local afterSequence = math.max(0, math.floor(finiteNumber(args.afterSeq or args.afterSequence, 0)))
	local waitSeconds = math.clamp(finiteNumber(args.waitSeconds, 0), 0, 2.5)
	local maximumEvents = math.clamp(math.floor(finiteNumber(args.maxEvents, POLL_MAX_EVENTS)), 1, POLL_MAX_EVENTS)
	local maximumBytes = math.clamp(math.floor(finiteNumber(args.maxBytes, POLL_MAX_BYTES)), 1024, POLL_MAX_BYTES)
	local deadline = os.clock() + waitSeconds
	repeat
		self:_flushQueueDropped(job)
		local available = false
		for _, event in ipairs(run._events) do
			if event.seq > afterSequence then
				available = true
				break
			end
		end
		if available or run.terminal or waitSeconds <= 0 then
			break
		end
		task.wait(0.05)
	until os.clock() >= deadline

	for index = #run._events, 1, -1 do
		local event = run._events[index]
		if event.seq <= afterSequence then
			run._eventBytes -= event.__bytes or 0
			table.remove(run._events, index)
		end
	end
	local events = {}
	local bytes = 0
	local lastDelivered = afterSequence
	local hasMore = false
	for _, stored in ipairs(run._events) do
		if stored.seq > afterSequence then
			local eventBytes = stored.__bytes or self:_eventBytes(stored)
			if #events >= maximumEvents or (#events > 0 and bytes + eventBytes > maximumBytes) then
				hasMore = true
				break
			end
			local event = table.clone(stored)
			event.__bytes = nil
			table.insert(events, event)
			bytes += eventBytes
			lastDelivered = event.seq
		end
	end
	local heartbeats = {}
	for name, heartbeat in pairs(run._heartbeats) do
		heartbeats[name] = {
			lastSeen = heartbeat,
			ageSeconds = math.max(0, os.clock() - heartbeat),
		}
	end
	return {
		frames = events,
		nextSeq = lastDelivered,
		lastSeq = lastDelivered,
		hasMore = hasMore,
		heartbeats = heartbeats,
		heartbeatStale = run._heartbeatStale,
		run = self:_snapshot(job),
	}
end

function Coordinator:setContexts(job, contexts)
	if job and job.run then
		job.run._contexts = contexts
	end
end

function Coordinator:cancelRunners(job, contexts, reason)
	for _, context in pairs(contexts or {}) do
		if context.playtestJobId == job.id and context.connection.Connected then
			self:_send(context, "cancel", { reason = tostring(reason or "cancelled") })
		end
	end
end

Playscript.constants = {
	frameKind = FRAME_KIND,
	resultChunkBytes = RESULT_CHUNK_BYTES,
	eventBytes = EVENT_BYTES,
	resultBytes = RESULT_BYTES,
	eventRate = EVENT_RATE,
	eventBurst = EVENT_BURST,
	pollEvents = POLL_MAX_EVENTS,
	pollBytes = POLL_MAX_BYTES,
}

return Playscript
