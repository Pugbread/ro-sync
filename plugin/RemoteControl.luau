--!nonstrict
-- Ro Sync — remote-control (CLI op) surface for the Studio plugin.
--
-- Packaged as a ModuleScript child of the RoSync plugin Script so the main
-- chunk stays below Luau's 200-register-per-function ceiling. Everything this
-- surface used to close over in Plugin.luau is injected through `deps`;
-- sibling packaged modules (Clipboard / Photo / Playscript) are reached
-- through `script.Parent`.

--------------------------------------------------------------------------------
-- Remote-control request/response protocol (CLI-backed)
--------------------------------------------------------------------------------
-- Schema:
--   C→S (from daemon, initiated by CLI):
--     { type = "request", request_id = <uint>, op = <string>, args = <table> }
--   S→C (this plugin):
--     { type = "response", request_id = <uint>, ok = true,  value = <any>  }
--     { type = "response", request_id = <uint>, ok = false, error = <str>  }
--
-- Ops:
--   get   args: { path = "Workspace/Foo", prop = "Name" }
--           value: codec-encoded property value
--   set   args: { path, prop, value = <codec-encoded> }
--           value: { path, prop }
--   ls    args: { path = "" | "/" | "Workspace/..." }
--           value: [ { class, name, path }, ... ] (shallow children)
--   tree  args: { path?, depth? = 3 }
--           value: { class, name, children = [ ... ] } nested; leaves beyond
--                  depth carry truncated = true if they had children.
--   find  args: { root?, className?, name?, limit? = 500 }
--           value: [ "path", "path", ... ]
--   eval  args: { source = "<luau>" }
--           value: codec-encoded return value(s); compile/runtime errors are
--                  surfaced as ok=false responses.
--
-- Paths are DataModel-rooted and "/"-separated. "" or "/" resolves to `game`.
-- The first segment is tried as a service first, then as a direct child.
--
-- JSON codec:
--   nil / boolean / number / string  → pass through (NaN/Inf stripped)
--   Vector2/3, CFrame, Color3, UDim, UDim2, EnumItem, BrickColor,
--   NumberRange, Instance → tagged { __type = "...", ... } objects
--   tables → recurse; arrays preserved; mixed/object tables keyed by string
--
-- Writes (set / eval) also POST a one-line JSON entry to /writelog. The
-- daemon is responsible for appending to ~/.terminal64/widgets/ro-sync/writes.log
-- (Studio plugins cannot write arbitrary files). POST failures warn but never
-- fail the op.
--
-- 5s timeout is enforced on the daemon side; the plugin always replies when
-- the handler returns.

local RemoteControl = {}

function RemoteControl.create(deps)
	local HttpService = deps.HttpService
	local ChangeHistoryService = deps.ChangeHistoryService
	local AssetService = deps.AssetService
	local EncodingService = deps.EncodingService
	local plugin = deps.plugin
	local sha256Hex = deps.sha256Hex
	local httpJson = deps.httpJson
	local readScriptSource = deps.readScriptSource
	local writeScriptSource = deps.writeScriptSource
	local resolvePath = deps.resolvePath
	local readLogRing = deps.readLogRing
	local wsSend = deps.wsSend
	local log = deps.log
	local dbg = deps.dbg
	local hostDataModelTypeName = deps.hostDataModelTypeName
	local getWsGeneration = deps.getWsGeneration
	local loadPluginChunk = deps.loadPluginChunk
	local PLUGIN_PROTOCOL_VERSION = deps.PLUGIN_PROTOCOL_VERSION
	local PLUGIN_VERSION_STRING = deps.PLUGIN_VERSION_STRING

	local TREE_DEPTH_DEFAULT = 3
	local FIND_LIMIT_DEFAULT = 500
	local EVAL_CHUNK_NAME = "rosync-eval"

	local function splitRemotePath(p: string): { string }
		local segs = {}
		if p == nil or p == "" or p == "/" then
			return segs
		end
		for seg in string.gmatch(tostring(p), "[^/]+") do
			table.insert(segs, seg)
		end
		return segs
	end

	local function resolveRemotePath(p: string): Instance?
		local segs = splitRemotePath(p)
		if #segs == 0 then
			return game
		end
		return resolvePath(segs)
	end

	local function instancePathString(inst: Instance): string
		if inst == nil or inst == game then
			return ""
		end
		local parts = {}
		local cur = inst
		while cur and cur ~= game do
			table.insert(parts, 1, cur.Name)
			cur = cur.Parent
		end
		return table.concat(parts, "/")
	end

	local encodeRemoteValue
	local REMOTE_ENCODE_MAX_DEPTH = 64
	local REMOTE_ENCODE_MAX_NODES = 20000
	local REMOTE_ENCODE_MAX_TABLE_ENTRIES = 10000
	local REMOTE_ENCODE_MAX_STRING_BYTES = 4 * 1024 * 1024

	function encodeRemoteValue(v, depth, state)
		depth = depth or 0
		state = state or { nodes = 0, stringBytes = 0, seen = {} }
		if depth > REMOTE_ENCODE_MAX_DEPTH then
			error("remote value exceeds maximum nesting depth")
		end
		state.nodes += 1
		if state.nodes > REMOTE_ENCODE_MAX_NODES then
			error("remote value exceeds maximum node count")
		end
		local t = typeof(v)
		if t == "string" then
			state.stringBytes += #v
			if state.stringBytes > REMOTE_ENCODE_MAX_STRING_BYTES then
				error("remote value exceeds maximum string byte count")
			end
			return v
		elseif t == "nil" or t == "boolean" then
			return v
		elseif t == "number" then
			if v ~= v or v == math.huge or v == -math.huge then
				return nil
			end
			return v
		elseif t == "Vector2" then
			return { __type = "Vector2", x = v.X, y = v.Y }
		elseif t == "Vector3" then
			return { __type = "Vector3", x = v.X, y = v.Y, z = v.Z }
		elseif t == "CFrame" then
			return { __type = "CFrame", components = { v:GetComponents() } }
		elseif t == "Color3" then
			return { __type = "Color3", r = v.R, g = v.G, b = v.B }
		elseif t == "UDim" then
			return { __type = "UDim", scale = v.Scale, offset = v.Offset }
		elseif t == "UDim2" then
			return {
				__type = "UDim2",
				x = { scale = v.X.Scale, offset = v.X.Offset },
				y = { scale = v.Y.Scale, offset = v.Y.Offset },
			}
		elseif t == "EnumItem" then
			return { __type = "EnumItem", enum = tostring(v.EnumType), name = v.Name, value = v.Value }
		elseif t == "BrickColor" then
			return { __type = "BrickColor", name = v.Name }
		elseif t == "NumberRange" then
			return { __type = "NumberRange", min = v.Min, max = v.Max }
		elseif t == "Instance" then
			return { __type = "Instance", path = instancePathString(v), class = v.ClassName }
		elseif t == "table" then
			if state.seen[v] then
				error("remote value contains a table cycle")
			end
			state.seen[v] = true
			local isArray = true
			local entryCount = 0
			for k in pairs(v) do
				entryCount += 1
				if entryCount > REMOTE_ENCODE_MAX_TABLE_ENTRIES then
					state.seen[v] = nil
					error("remote table exceeds maximum entry count")
				end
				if type(k) ~= "number" then
					isArray = false
					break
				end
			end
			if isArray then
				local out = {}
				for i, item in ipairs(v) do
					out[i] = encodeRemoteValue(item, depth + 1, state)
				end
				state.seen[v] = nil
				return out
			else
				local out = {}
				for k, item in pairs(v) do
					if type(k) == "string" then
						out[k] = encodeRemoteValue(item, depth + 1, state)
					end
				end
				state.seen[v] = nil
				return out
			end
		end
		return tostring(v)
	end

	local function decodeRemoteValue(v)
		local t = type(v)
		if t == "nil" or t == "boolean" or t == "string" or t == "number" then
			return v
		end
		if t ~= "table" then
			return v
		end
		local tag = v.__type
		if tag == nil then
			return v
		end -- untagged plain table passes through
		if tag == "Vector2" then
			return Vector2.new(tonumber(v.x) or 0, tonumber(v.y) or 0)
		elseif tag == "Vector3" then
			return Vector3.new(tonumber(v.x) or 0, tonumber(v.y) or 0, tonumber(v.z) or 0)
		elseif tag == "CFrame" then
			local c = v.components or {}
			-- Accept either the 3-arg (pos only) or 12-arg full matrix form.
			if #c == 3 then
				return CFrame.new(c[1], c[2], c[3])
			end
			if #c == 12 then
				return CFrame.new(c[1], c[2], c[3], c[4], c[5], c[6], c[7], c[8], c[9], c[10], c[11], c[12])
			end
			error("CFrame components must have length 3 or 12 (got " .. #c .. ")")
		elseif tag == "Color3" then
			return Color3.new(tonumber(v.r) or 0, tonumber(v.g) or 0, tonumber(v.b) or 0)
		elseif tag == "UDim" then
			return UDim.new(tonumber(v.scale) or 0, tonumber(v.offset) or 0)
		elseif tag == "UDim2" then
			local x = v.x or {}
			local y = v.y or {}
			return UDim2.new(
				tonumber(x.scale) or 0,
				tonumber(x.offset) or 0,
				tonumber(y.scale) or 0,
				tonumber(y.offset) or 0
			)
		elseif tag == "EnumItem" then
			local enumName = tostring(v.enum or "")
			local enumType = Enum[enumName]
			if not enumType then
				error("unknown Enum: " .. enumName)
			end
			local ok, item = pcall(function()
				return enumType[tostring(v.name or "")]
			end)
			if not ok or item == nil then
				error("unknown EnumItem: " .. enumName .. "." .. tostring(v.name))
			end
			return item
		elseif tag == "BrickColor" then
			return BrickColor.new(tostring(v.name or "Medium stone grey") :: any)
		elseif tag == "NumberRange" then
			return NumberRange.new(tonumber(v.min) or 0, tonumber(v.max) or 0)
		elseif tag == "Instance" then
			local inst = resolveRemotePath(tostring(v.path or ""))
			if not inst then
				error("instance not found: " .. tostring(v.path))
			end
			return inst
		end
		return v
	end

	local remoteHandlers = {}
	-- Remote feature helpers are deliberately split into short lexical scopes.
	-- Luau limits every function (including this top-level chunk) to 200 local
	-- registers; keeping all handler helpers alive until EOF makes the packaged
	-- plugin fail to compile in Studio as the command surface grows.
	local remoteState: any = {}

	do
		-- Keep the native instance clipboard in a packaged ModuleScript so this
		-- already-large top-level chunk stays below Luau's 200-register ceiling.
		remoteState.clipboardAvailable = false
		local moduleScript = script.Parent:FindFirstChild("Clipboard")
		if moduleScript and moduleScript:IsA("ModuleScript") then
			local okModule, module = pcall(require, moduleScript)
			if okModule and type(module) == "table" and type(module.install) == "function" then
				local okInstall, controller = pcall(module.install, {
					resolvePath = resolveRemotePath,
					pathOf = instancePathString,
					httpJson = httpJson,
					readScriptSource = readScriptSource,
				})
				if okInstall and type(controller) == "table" and type(controller.handlers) == "table" then
					remoteState.clipboardAvailable = controller.available == true
					for name, handler in pairs(controller.handlers) do
						remoteHandlers[name] = handler
					end
				else
					warn("[Ro Sync] native clipboard unavailable: " .. tostring(controller))
				end
			else
				warn("[Ro Sync] packaged Clipboard module failed to load: " .. tostring(module))
			end
		end
	end

	do
		-- ---------------------------------------------------------------------------
		-- Edit-mode playtest coordinator. Runtime copies of this plugin advertise
		-- themselves over PluginConnectionService; this registry assigns friendly
		-- context names (`server`, `client:1`, ...) and routes correlated requests.
		-- ---------------------------------------------------------------------------

		local runtimeContextsById = {}
		local runtimeContextsByName = {}
		local runtimePendingRequests = {}
		local boundRuntimeConnections = setmetatable({}, { __mode = "k" })
		local nextRuntimeClientIndex = 1
		local runtimeRequestCounter = 0
		local playtestJobs = {}
		local playscriptRunsByClientId = {}
		local playscriptRunTombstones = {}
		local latestPlaytestJobId = nil
		local requestRuntimeContext
		local fetchPlayscriptResult
		local playscriptCoordinator = nil
		do
			local moduleScript = script.Parent:FindFirstChild("Playscript")
			if moduleScript and moduleScript:IsA("ModuleScript") then
				local okModule, module = pcall(require, moduleScript)
				if okModule and type(module) == "table" and type(module.newCoordinator) == "function" then
					local okCoordinator, coordinator = pcall(module.newCoordinator, {
						httpService = HttpService,
						sha256Hex = sha256Hex,
						onTerminalRequested = function(job, terminal, external)
							local callback = remoteState.onPlayscriptTerminalRequested
							if callback then
								callback(job, terminal, external)
							end
						end,
					})
					if okCoordinator then
						playscriptCoordinator = coordinator
					else
						warn("[Ro Sync] playscript coordinator initialization failed: " .. tostring(coordinator))
					end
				end
			end
		end

		local function coordinatorAuth(sessionToken: string, purpose: string): string
			return sha256Hex(sessionToken .. "|" .. purpose)
		end

		local function runtimeContextSnapshot(context)
			return {
				id = context.name,
				runtimeId = context.runtimeId,
				playtestJobId = context.playtestJobId,
				role = context.role,
				hostDataModelType = context.hostDataModelType,
				rawHostDataModelType = context.rawHostDataModelType,
				canConnectToEdit = context.canConnectToEdit,
				canConnectToTest = context.canConnectToTest,
				testArgsAuthenticated = context.testArgsAuthenticated,
				placeId = context.placeId,
				gameId = context.gameId,
				jobId = context.jobId,
				playerName = context.playerName,
				userId = context.userId,
				targetId = context.targetId,
				connected = context.connection.Connected,
				lastSeen = context.lastSeen,
			}
		end

		local function pruneRuntimeContexts()
			local changedJobs = {}
			for runtimeId, context in pairs(runtimeContextsById) do
				if not context.connection.Connected then
					changedJobs[context.playtestJobId] = true
					runtimeContextsById[runtimeId] = nil
					if runtimeContextsByName[context.name] == context then
						runtimeContextsByName[context.name] = nil
					end
				end
			end
			if playscriptCoordinator then
				for jobId in pairs(changedJobs) do
					local job = playtestJobs[jobId]
					if job and job.run then
						playscriptCoordinator:setContexts(job, runtimeContextsByName)
						playscriptCoordinator:onContextsChanged(job, runtimeContextsByName)
					end
				end
			end
		end

		local function registerRuntimeHello(connection, message)
			local runtimeId = tostring(message.runtimeId or "")
			if runtimeId == "" then
				return
			end
			local playtestJobId = tostring(message.playtestJobId or "")
			local job = playtestJobs[playtestJobId]
			if
				job == nil
				or job.sessionToken == nil
				or (job.status ~= "starting" and job.status ~= "running" and job.status ~= "stopping")
				or tonumber(message.protocolVersion) ~= PLUGIN_PROTOCOL_VERSION
				or tostring(message.auth or "") ~= coordinatorAuth(job.sessionToken, "hello|" .. runtimeId)
			then
				return
			end
			local existing = runtimeContextsById[runtimeId]
			local name
			if existing then
				name = existing.name
			elseif tostring(message.role) == "server" and runtimeContextsByName.server == nil then
				name = "server"
			else
				repeat
					name = "client:" .. tostring(nextRuntimeClientIndex)
					nextRuntimeClientIndex += 1
				until runtimeContextsByName[name] == nil
			end
			local context = {
				name = name,
				runtimeId = runtimeId,
				playtestJobId = playtestJobId,
				sessionToken = job.sessionToken,
				role = tostring(message.role or "unknown"),
				hostDataModelType = tostring(message.hostDataModelType or "Unknown"),
				rawHostDataModelType = tostring(message.rawHostDataModelType or "Unknown"),
				canConnectToEdit = message.canConnectToEdit == true,
				canConnectToTest = message.canConnectToTest == true,
				testArgsAuthenticated = message.testArgsAuthenticated == true,
				placeId = message.placeId,
				gameId = message.gameId,
				jobId = message.jobId,
				playerName = message.playerName,
				userId = message.userId,
				targetId = connection.TargetId,
				connection = connection,
				lastSeen = os.clock(),
			}
			runtimeContextsById[runtimeId] = context
			runtimeContextsByName[name] = context
			if playscriptCoordinator and job.run then
				playscriptCoordinator:setContexts(job, runtimeContextsByName)
				playscriptCoordinator:onContext(job, context, runtimeContextsByName)
			end
		end

		local function bindRuntimeConnection(connection)
			if boundRuntimeConnections[connection] then
				return
			end
			boundRuntimeConnections[connection] = true
			connection:BindToMessage(function(payload)
				local ok, message = pcall(function()
					return HttpService:JSONDecode(tostring(payload))
				end)
				if not ok or type(message) ~= "table" then
					return
				end
				if message.kind == "rosync-runtime-hello" then
					registerRuntimeHello(connection, message)
					return
				end
				if message.kind == "rosync-playscript-frame" then
					local runtimeId = tostring(message.runtimeId or "")
					local context = runtimeContextsById[runtimeId]
					if context and context.connection == connection then
						local job = playtestJobs[context.playtestJobId]
						if job and job.run and playscriptCoordinator then
							local action = playscriptCoordinator:handleFrame(job, context, message)
							if action then
								task.spawn(function()
									if fetchPlayscriptResult then
										fetchPlayscriptResult(action)
									else
										playscriptCoordinator:resolveResult(action, nil, "result transport unavailable")
									end
								end)
							end
						end
					end
					return
				end
				if message.kind == "rosync-runtime-response" then
					local requestId = tostring(message.requestId or "")
					local pending = runtimePendingRequests[requestId]
					if
						pending
						and pending.connection == connection
						and tostring(message.auth or "") == pending.expectedAuth
					then
						pending.response = message
						pending.event:Fire()
					end
				end
			end)
			pcall(function()
				connection:SendMessage(HttpService:JSONEncode({ kind = "rosync-runtime-discover" }))
			end)
		end

		do
			local ok, service = pcall(function()
				return game:GetService("PluginConnectionService")
			end)
			if ok and service then
				pcall(function()
					for _, connection in
						ipairs(service:GetPluginConnectionsOfType(Enum.PluginConnectionTargetType.Test))
					do
						bindRuntimeConnection(connection)
					end
				end)
				service.Connected:Connect(function(connection)
					if connection.Type == Enum.PluginConnectionTargetType.Test then
						bindRuntimeConnection(connection)
					end
				end)
			end
		end

		requestRuntimeContext = function(context, operation: string, args: any, timeoutSeconds: number)
			if not context or not context.connection.Connected then
				error("playtest context is not connected")
			end
			runtimeRequestCounter += 1
			local requestId = HttpService:GenerateGUID(false) .. "-" .. tostring(runtimeRequestCounter)
			local pending = {
				event = Instance.new("BindableEvent"),
				response = nil,
				timedOut = false,
				connection = context.connection,
				expectedAuth = coordinatorAuth(context.sessionToken, "response|" .. requestId),
			}
			runtimePendingRequests[requestId] = pending
			local requestArgs = if type(args) == "table" then table.clone(args) else {}
			requestArgs.__rosyncTimeoutSeconds = timeoutSeconds
			local sent, sendError = pcall(function()
				context.connection:SendMessage(HttpService:JSONEncode({
					kind = "rosync-runtime-request",
					requestId = requestId,
					op = operation,
					auth = coordinatorAuth(context.sessionToken, "request|" .. requestId .. "|" .. operation),
					args = requestArgs,
				}))
			end)
			if not sent then
				runtimePendingRequests[requestId] = nil
				pending.event:Destroy()
				error("runtime send failed: " .. tostring(sendError))
			end
			task.delay(timeoutSeconds, function()
				if runtimePendingRequests[requestId] == pending and pending.response == nil then
					pending.timedOut = true
					pending.event:Fire()
				end
			end)
			pending.event.Event:Wait()
			runtimePendingRequests[requestId] = nil
			pending.event:Destroy()
			if pending.response == nil then
				error("runtime request timed out after " .. tostring(timeoutSeconds) .. "s")
			end
			context.lastSeen = os.clock()
			if pending.response.ok ~= true then
				local runtimeError = pending.response.error
				if type(runtimeError) == "table" then
					error(
						tostring(runtimeError.code or "RUNTIME_ERROR")
							.. ": "
							.. tostring(runtimeError.message or "failed")
					)
				end
				error(tostring(runtimeError or "runtime request failed"))
			end
			return pending.response.value
		end

		fetchPlayscriptResult = function(action)
			local context = action.context
			local result = action.result
			local resultId = tostring(result.id or "")
			local byteLength = math.floor(tonumber(result.bytes) or -1)
			if resultId == "" or byteLength < 2 or byteLength > 2 * 1024 * 1024 then
				playscriptCoordinator:resolveResult(action, nil, "runtime returned invalid playscript result metadata")
				return
			end
			local deadline = os.clock() + 20
			local pieces = {}
			local offset = 0
			local transferOk, transferError = pcall(function()
				while offset < byteLength do
					local remaining = deadline - os.clock()
					if remaining <= 0 then
						error("playscript result transfer timed out")
					end
					local chunk = requestRuntimeContext(context, "playscript_result_read", {
						id = resultId,
						offset = offset,
						maxBytes = 96 * 1024,
					}, math.max(0.1, math.min(remaining, 5)))
					local chunkOffset = math.floor(tonumber(chunk.offset) or -1)
					local nextOffset = math.floor(tonumber(chunk.nextOffset) or -1)
					if chunkOffset ~= offset or nextOffset <= offset or nextOffset > byteLength then
						error("playscript result returned an invalid chunk offset")
					end
					local decoded = EncodingService:Base64Decode(buffer.fromstring(tostring(chunk.bytesBase64 or "")))
					if typeof(decoded) == "buffer" then
						decoded = buffer.tostring(decoded)
					end
					if type(decoded) ~= "string" or #decoded ~= nextOffset - offset then
						error("playscript result returned an invalid chunk length")
					end
					table.insert(pieces, decoded)
					offset = nextOffset
				end
				local encoded = table.concat(pieces)
				if #encoded ~= byteLength then
					error("playscript result length mismatch")
				end
				if tostring(result.sha256 or "") ~= sha256Hex(encoded) then
					error("playscript result SHA-256 mismatch")
				end
				local decodeOk, envelope = pcall(function()
					return HttpService:JSONDecode(encoded)
				end)
				if not decodeOk or type(envelope) ~= "table" then
					error("playscript result is not a valid JSON envelope")
				end
				playscriptCoordinator:resolveResult(action, envelope, nil)
			end)
			pcall(function()
				requestRuntimeContext(context, "playscript_result_close", { id = resultId }, 1)
			end)
			if not transferOk then
				playscriptCoordinator:resolveResult(action, nil, tostring(transferError))
			end
		end

		local function playtestJobSnapshot(job)
			if not job then
				return nil
			end
			return {
				id = job.id,
				clientRunId = job.clientRunId,
				mode = job.mode,
				players = job.players,
				status = job.status,
				createdAt = job.createdAt,
				startedAt = job.startedAt,
				endedAt = job.endedAt,
				result = job.result,
				error = job.error,
			}
		end

		local function playscriptFingerprint(args)
			local main = if type(args.script) == "table" then args.script else {}
			local client = if type(args.clientScript) == "table" then args.clientScript else nil
			local mainSource = tostring(main.source or args.source or "")
			local clientSource = if client then tostring(client.source or "") else ""
			local argsJson = tostring(args.scriptArgsJson or "")
			if argsJson == "" then
				local argsOk, encodedArgs = pcall(function()
					return HttpService:JSONEncode(if args.scriptArgs == nil then {} else args.scriptArgs)
				end)
				if not argsOk then
					error("playtest run args could not be fingerprinted: " .. tostring(encodedArgs))
				end
				argsJson = encodedArgs
			end
			local testArgsOk, testArgsJson = pcall(function()
				return HttpService:JSONEncode(if type(args.testArgs) == "table" then args.testArgs else {})
			end)
			if not testArgsOk then
				error("playtest run test args could not be fingerprinted: " .. tostring(testArgsJson))
			end
			local fields = {
				tostring(args.mode or "play"):lower(),
				tostring(math.clamp(math.floor(tonumber(args.players) or 1), 1, 8)),
				tostring(args.context or "server"),
				tostring(args.identity or "game"):lower(),
				tostring(math.clamp(tonumber(args.timeout) or 600, 0.1, 3600)),
				tostring(args.logs or "off"):lower(),
				if args.keepOpen == true then "true" else "false",
				tostring(main.path or ""),
				sha256Hex(mainSource),
				if clientSource == "" then "" else tostring(client.path or ""),
				if clientSource == "" then "" else sha256Hex(clientSource),
				argsJson,
				testArgsJson,
			}
			local framed = {}
			for _, field in ipairs(fields) do
				table.insert(framed, tostring(#field) .. ":" .. field)
			end
			return sha256Hex(table.concat(framed, "|"))
		end

		local function prunePlayscriptRunKeys()
			local now = os.clock()
			for clientRunId, entry in pairs(playscriptRunsByClientId) do
				local status = tostring(entry.job.status)
				local active = status == "starting" or status == "running" or status == "stopping"
				if not active and now - entry.lastUsed > 600 then
					playscriptRunsByClientId[clientRunId] = nil
				end
			end
			for clientRunId, tombstone in pairs(playscriptRunTombstones) do
				if now - tombstone.createdAt > 120 then
					playscriptRunTombstones[clientRunId] = nil
				end
			end
			while true do
				local count = 0
				local oldestId = nil
				local oldestClock = math.huge
				for clientRunId, entry in pairs(playscriptRunsByClientId) do
					count += 1
					local status = tostring(entry.job.status)
					local active = status == "starting" or status == "running" or status == "stopping"
					if not active and entry.lastUsed < oldestClock then
						oldestClock = entry.lastUsed
						oldestId = clientRunId
					end
				end
				if count <= 64 or not oldestId then
					break
				end
				playscriptRunsByClientId[oldestId] = nil
			end
			while true do
				local count = 0
				local oldestId = nil
				local oldestClock = math.huge
				for clientRunId, tombstone in pairs(playscriptRunTombstones) do
					count += 1
					if tombstone.createdAt < oldestClock then
						oldestClock = tombstone.createdAt
						oldestId = clientRunId
					end
				end
				if count <= 64 or not oldestId then
					break
				end
				playscriptRunTombstones[oldestId] = nil
			end
		end

		local function playscriptJobForArgs(args)
			local explicitJobId = tostring(args.jobId or "")
			local clientRunId = tostring(args.clientRunId or "")
			if clientRunId ~= "" and (#clientRunId > 128 or string.find(clientRunId, "%c")) then
				error("playtest run clientRunId must be at most 128 bytes without control characters")
			end
			if explicitJobId ~= "" then
				if clientRunId ~= "" then
					prunePlayscriptRunKeys()
					local entry = playscriptRunsByClientId[clientRunId]
					if not entry or entry.job.id ~= explicitJobId then
						error("playtest jobId does not match clientRunId")
					end
					entry.lastUsed = os.clock()
					return explicitJobId, entry.job, clientRunId
				end
				return explicitJobId, playtestJobs[explicitJobId], clientRunId
			end
			if clientRunId ~= "" then
				prunePlayscriptRunKeys()
				local entry = playscriptRunsByClientId[clientRunId]
				if entry then
					entry.lastUsed = os.clock()
					return entry.job.id, entry.job, clientRunId
				end
				return "", nil, clientRunId
			end
			local jobId = tostring(latestPlaytestJobId or "")
			return jobId, playtestJobs[jobId], ""
		end

		local function startPlaytestJob(args, runConfiguration)
			for _, job in pairs(playtestJobs) do
				if job.status == "starting" or job.status == "running" or job.status == "stopping" then
					error("a playtest job is already active: " .. job.id)
				end
			end
			local service = game:GetService("StudioTestService")
			local mode = tostring(args.mode or "play"):lower()
			if mode ~= "play" and mode ~= "run" and mode ~= "multiplayer" then
				error("playtest mode must be play, run, or multiplayer")
			end
			local players = math.clamp(math.floor(tonumber(args.players) or 1), 1, 8)
			local jobId = HttpService:GenerateGUID(false)
			local sessionToken = HttpService:GenerateGUID(false) .. HttpService:GenerateGUID(false)
			local previousLatestJobId = latestPlaytestJobId
			local job = {
				id = jobId,
				clientRunId = if runConfiguration then tostring(runConfiguration.clientRunId or "") else nil,
				mode = mode,
				players = players,
				status = "starting",
				createdAt = os.time(),
				sessionToken = sessionToken,
				stopRequested = false,
			}
			playtestJobs[jobId] = job
			latestPlaytestJobId = jobId
			runtimeContextsById = {}
			runtimeContextsByName = {}
			nextRuntimeClientIndex = 1
			if runConfiguration ~= nil then
				if not playscriptCoordinator then
					playtestJobs[jobId] = nil
					latestPlaytestJobId = previousLatestJobId
					error("packaged playscript coordinator is unavailable; reinstall the Studio plugin")
				end
				local configured, configureResult = pcall(function()
					return playscriptCoordinator:createRun(job, runConfiguration)
				end)
				if not configured then
					playtestJobs[jobId] = nil
					latestPlaytestJobId = previousLatestJobId
					error(tostring(configureResult))
				end
			end
			local historicalJobs = {}
			for id, candidate in pairs(playtestJobs) do
				if
					id ~= jobId
					and candidate.status ~= "starting"
					and candidate.status ~= "running"
					and candidate.status ~= "stopping"
				then
					table.insert(historicalJobs, candidate)
				end
			end
			if #historicalJobs > 20 then
				table.sort(historicalJobs, function(a, b)
					return (a.createdAt or 0) < (b.createdAt or 0)
				end)
				for index = 1, #historicalJobs - 20 do
					playtestJobs[historicalJobs[index].id] = nil
				end
			end
			task.spawn(function()
				job.status = "running"
				job.startedAt = os.time()
				local ok, result = pcall(function()
					local testArgs = if type(args.testArgs) == "table" then table.clone(args.testArgs) else {}
					testArgs.__rosyncSessionToken = sessionToken
					testArgs.__rosyncJobId = jobId
					if mode == "run" then
						return service:ExecuteRunModeAsync(testArgs)
					elseif mode == "multiplayer" then
						-- Studio's one-player multiplayer launcher creates only a
						-- PlayClient DataModel, so there is no server context in which
						-- to install the main playscript. Play mode provides the same
						-- one-server/one-client topology while preserving the requested
						-- mode in the run configuration exposed to playscripts.
						if players == 1 then
							return service:ExecutePlayModeAsync(testArgs)
						end
						return service:ExecuteMultiplayerTestAsync(players, testArgs)
					else
						return service:ExecutePlayModeAsync(testArgs)
					end
				end)
				job.endedAt = os.time()
				if job.stopRequested then
					job.status = "stopped"
					if ok then
						local encodedOk, encoded = pcall(encodeRemoteValue, result)
						if encodedOk then
							job.result = encoded
						else
							job.stopDetail = "result encoding failed: " .. tostring(encoded)
						end
					else
						job.stopDetail = tostring(result)
					end
				elseif ok then
					local encodedOk, encoded = pcall(encodeRemoteValue, result)
					if encodedOk then
						job.status = "completed"
						job.result = encoded
					else
						job.status = "failed"
						job.error = "result encoding failed: " .. tostring(encoded)
					end
				else
					job.status = "failed"
					job.error = tostring(result)
				end
				pruneRuntimeContexts()
				if playscriptCoordinator and job.run then
					playscriptCoordinator:onJobEnded(job)
				end
			end)
			return job
		end

		remoteHandlers.playtest_start = function(args)
			return playtestJobSnapshot(startPlaytestJob(args, nil))
		end

		remoteHandlers.playtest_run_start = function(args)
			local clientRunId = tostring(args.clientRunId or "")
			if clientRunId == "" or #clientRunId > 128 or string.find(clientRunId, "%c") then
				error("playtest run clientRunId must be 1-128 bytes without control characters")
			end
			prunePlayscriptRunKeys()
			if playscriptRunTombstones[clientRunId] then
				if remoteState.preAuditedWrites then
					remoteState.preAuditedWrites[args] = true
				end
				error("playtest run was cancelled before launch: " .. clientRunId)
			end
			local fingerprint = playscriptFingerprint(args)
			local existing = playscriptRunsByClientId[clientRunId]
			if existing then
				if existing.fingerprint ~= fingerprint then
					error("playtest run clientRunId was already used with different content")
				end
				existing.lastUsed = os.clock()
				if remoteState.preAuditedWrites then
					remoteState.preAuditedWrites[args] = true
				end
				return {
					job = playtestJobSnapshot(existing.job),
					run = playscriptCoordinator:snapshot(existing.job),
					reused = true,
					replayed = true,
					clientRunId = clientRunId,
				}
			end
			local job = startPlaytestJob(args, args)
			playscriptRunsByClientId[clientRunId] = {
				job = job,
				fingerprint = fingerprint,
				createdAt = os.clock(),
				lastUsed = os.clock(),
			}
			prunePlayscriptRunKeys()
			args.__rosyncAuditJobId = job.id
			local response = {
				job = playtestJobSnapshot(job),
				run = playscriptCoordinator:snapshot(job),
				reused = false,
				replayed = false,
				clientRunId = clientRunId,
			}
			local postAudit = remoteState.postRemoteWriteLogSync
			local preAuditedWrites = remoteState.preAuditedWrites
			if postAudit and preAuditedWrites then
				preAuditedWrites[args] = true
				local auditOk = postAudit({
					ts = os.time(),
					op = "playtest_run_start",
					args = playscriptCoordinator:auditStart(job),
					ok = true,
					result = response,
				})
				if auditOk then
					preAuditedWrites[args] = true
				else
					preAuditedWrites[args] = nil
				end
			end
			return response
		end

		remoteState.auditArgsForOp = function(op, args)
			if op == "clipboard_copy" then
				return {
					selection = type(args.paths) ~= "table" or #args.paths == 0,
					paths = if type(args.paths) == "table" then args.paths else nil,
					pathCount = if type(args.paths) == "table" then #args.paths else 0,
				}
			elseif op == "clipboard_paste" then
				return {
					parent = tostring(args.parent or ""),
					rootCount = if type(args.roots) == "table" then #args.roots else 0,
					select = args.select ~= false,
					byteLength = math.floor(tonumber(args.byteLength) or 0),
				}
			elseif op ~= "playtest_run_start" then
				return args
			end
			local main = if type(args.script) == "table" then args.script else {}
			local client = if type(args.clientScript) == "table" then args.clientScript else nil
			local mainSource = tostring(main.source or args.source or "")
			local clientSource = if client then tostring(client.source or "") else ""
			return {
				subcommand = "playtest run",
				jobId = args.__rosyncAuditJobId,
				clientRunId = tostring(args.clientRunId or ""),
				mode = tostring(args.mode or "play"),
				players = math.floor(tonumber(args.players) or 1),
				context = tostring(args.context or "server"),
				identity = tostring(args.identity or "game"),
				timeout = tonumber(args.timeout) or 600,
				keepOpen = args.keepOpen == true,
				args = args.scriptArgs,
				argsJson = tostring(args.scriptArgsJson or ""),
				script = {
					path = tostring(main.path or ""),
					sha256 = sha256Hex(mainSource),
					bytes = #mainSource,
				},
				clientScript = if client
					then {
						path = tostring(client.path or ""),
						sha256 = sha256Hex(clientSource),
						bytes = #clientSource,
					}
					else nil,
			}
		end
		remoteState.auditResultForOp = function(op, result)
			if op == "clipboard_copy" then
				return {
					rootCount = if type(result.roots) == "table" then #result.roots else 0,
					byteLength = math.floor(tonumber(result.byteLength) or 0),
				}
			elseif op == "clipboard_paste" then
				return {
					count = math.floor(tonumber(result.count) or 0),
					paths = result.paths,
					parent = result.parent,
					selected = result.selected == true,
				}
			end
			return result
		end

		local function runtimeContextsForJob(playtestJobId: string)
			pruneRuntimeContexts()
			local contexts = {}
			for _, context in pairs(runtimeContextsByName) do
				if context.playtestJobId == playtestJobId then
					table.insert(contexts, runtimeContextSnapshot(context))
				end
			end
			table.sort(contexts, function(a, b)
				return a.id < b.id
			end)
			return contexts
		end

		remoteHandlers.playtest_status = function(args)
			local jobId, job, clientRunId = playscriptJobForArgs(args)
			if jobId == "" then
				return { active = false, contexts = {}, clientRunId = clientRunId }
			end
			if not job then
				error("playtest job not found: " .. jobId)
			end
			local contexts = runtimeContextsForJob(jobId)
			return {
				active = job.status == "starting" or job.status == "running" or job.status == "stopping",
				job = playtestJobSnapshot(job),
				contexts = contexts,
				run = if playscriptCoordinator and job.run then playscriptCoordinator:snapshot(job) else nil,
			}
		end

		remoteHandlers.playtest_run_poll = function(args)
			local jobId, job = playscriptJobForArgs(args)
			if jobId == "" or not job or not job.run or not playscriptCoordinator then
				error("playtest run not found: " .. jobId)
			end
			playscriptCoordinator:setContexts(job, runtimeContextsByName)
			local result = playscriptCoordinator:poll(job, args)
			result.job = playtestJobSnapshot(job)
			return result
		end

		remoteHandlers.playtest_contexts = function(args)
			local jobId = tostring(args.jobId or latestPlaytestJobId or "")
			if jobId == "" then
				return { contexts = {}, count = 0 }
			end
			if not playtestJobs[jobId] then
				error("playtest job not found: " .. jobId)
			end
			local contexts = runtimeContextsForJob(jobId)
			return { contexts = contexts, count = #contexts }
		end

		remoteHandlers.playtest_wait = function(args)
			local jobId = tostring(args.jobId or latestPlaytestJobId or "")
			local job = playtestJobs[jobId]
			if jobId == "" or not job then
				error("no matching playtest job")
			end
			local minimum = math.clamp(math.floor(tonumber(args.minimum) or 1), 1, 9)
			local timeoutSeconds = math.clamp(tonumber(args.timeout) or 30, 0.1, 120)
			local deadline = os.clock() + timeoutSeconds
			repeat
				local contexts = runtimeContextsForJob(jobId)
				local count = #contexts
				if count >= minimum then
					return { contexts = contexts, count = count }
				end
				if job.status == "failed" or job.status == "completed" or job.status == "stopped" then
					error("playtest ended before the required runtime contexts connected: " .. tostring(job.status))
				end
				task.wait(0.05)
			until os.clock() >= deadline
			error("timed out waiting for " .. tostring(minimum) .. " playtest context(s)")
		end

		remoteHandlers.playtest_request = function(args)
			pruneRuntimeContexts()
			local contextName = tostring(args.context or "")
			local context = runtimeContextsByName[contextName]
			if not context then
				error("playtest context not found: " .. contextName)
			end
			local job = playtestJobs[context.playtestJobId]
			if job == nil or (job.status ~= "starting" and job.status ~= "running" and job.status ~= "stopping") then
				error("playtest context does not belong to an active job")
			end
			local operation = tostring(args.op or "")
			if operation == "" then
				error("missing runtime op")
			end
			local requestArgs = if type(args.args) == "table" then args.args else {}
			local timeoutSeconds = math.clamp(tonumber(args.timeout) or 15, 0.1, 120)
			return requestRuntimeContext(context, operation, requestArgs, timeoutSeconds)
		end

		remoteHandlers.playtest_capture = function(args)
			pruneRuntimeContexts()
			local contextName = tostring(args.context or "")
			local context = runtimeContextsByName[contextName]
			if not context then
				error("playtest context not found: " .. contextName)
			end
			local job = playtestJobs[context.playtestJobId]
			if job == nil or (job.status ~= "starting" and job.status ~= "running") then
				error("playtest context does not belong to an active job")
			end
			local timeoutSeconds = math.clamp(tonumber(args.timeout) or 30, 1, 120)
			local deadline = os.clock() + timeoutSeconds
			local function remainingTime(): number
				local remaining = deadline - os.clock()
				if remaining <= 0 then
					error("runtime capture exceeded its end-to-end deadline")
				end
				return math.max(0.1, remaining)
			end
			local options = if type(args.options) == "table" then args.options else {}
			local prepared = requestRuntimeContext(context, "capture_prepare", options, remainingTime())
			local byteLength = math.floor(tonumber(prepared.byteLength) or 0)
			if byteLength < 1 or byteLength > 128 * 1024 * 1024 then
				pcall(requestRuntimeContext, context, "capture_close", { sessionId = prepared.sessionId }, 1)
				error("runtime returned invalid capture byte length")
			end
			local okLease, leaseResponse = httpJson("/artifacts/lease", "POST", {
				filename = tostring(args.filename or (contextName:gsub("[^%w_-]", "-")) .. "-capture.png"),
				mime = "image/png",
				expectedSize = byteLength,
			})
			if not okLease or type(leaseResponse) ~= "table" or leaseResponse.ok ~= true then
				pcall(requestRuntimeContext, context, "capture_close", { sessionId = prepared.sessionId }, 1)
				error("unable to create runtime capture artifact lease: " .. tostring(leaseResponse))
			end
			local lease = leaseResponse.lease
			local transferOk, transferResult = pcall(function()
				local offset = 0
				while offset < byteLength do
					local chunk = requestRuntimeContext(context, "capture_read", {
						sessionId = prepared.sessionId,
						offset = offset,
						maxBytes = 384 * 1024,
					}, remainingTime())
					local nextOffset = math.floor(tonumber(chunk.nextOffset) or -1)
					if nextOffset <= offset or nextOffset > byteLength then
						error("runtime capture returned an invalid chunk offset")
					end
					local okChunk, chunkResponse = httpJson("/artifacts/" .. lease.id .. "/chunk", "POST", {
						token = lease.token,
						offset = offset,
						bytesBase64 = chunk.bytesBase64,
					})
					if not okChunk or type(chunkResponse) ~= "table" or chunkResponse.ok ~= true then
						error("runtime artifact chunk upload failed: " .. tostring(chunkResponse))
					end
					offset = nextOffset
				end
				remainingTime()
				local okFinalize, finalizeResponse = httpJson("/artifacts/" .. lease.id .. "/finalize", "POST", {
					token = lease.token,
				})
				if not okFinalize or type(finalizeResponse) ~= "table" or finalizeResponse.ok ~= true then
					error("runtime artifact finalization failed: " .. tostring(finalizeResponse))
				end
				return finalizeResponse.artifact
			end)
			pcall(requestRuntimeContext, context, "capture_close", { sessionId = prepared.sessionId }, 1)
			if not transferOk then
				pcall(function()
					httpJson("/artifacts/" .. lease.id .. "/abort", "POST", { token = lease.token })
				end)
				error(tostring(transferResult))
			end
			return {
				artifact = transferResult,
				capture = prepared,
				context = contextName,
			}
		end

		local function stopPlaytestJob(job, args, forceStop)
			local service = game:GetService("StudioTestService")
			local jobId = job.id
			if tostring(job.status) == "stopping" then
				error("playtest stop is already in progress")
			end
			if tostring(job.status) ~= "starting" and tostring(job.status) ~= "running" then
				return { stopped = true, alreadyEnded = true, job = playtestJobSnapshot(job) }
			end
			local retryableStatus = job.status
			local function restoreRetryableState()
				job.stopRequested = false
				if tostring(job.status) == "stopping" then
					job.status = retryableStatus
				end
			end
			local function failStop(message)
				local detail = tostring(message)
				if forceStop then
					-- Timeout and boot-failure cleanup are terminal requests. Studio can
					-- refuse every teardown API while a runtime context is unavailable;
					-- do not resurrect that generation as an active job forever.
					job.status = "failed"
					job.error = detail
					job.endedAt = os.time()
				else
					restoreRetryableState()
				end
				error(detail)
			end
			job.stopRequested = true
			job.status = "stopping"
			if playscriptCoordinator and job.run then
				playscriptCoordinator:cancelRunners(job, runtimeContextsByName, args.reason or "playtest stopped")
			end
			local serverContext = nil
			for _, context in pairs(runtimeContextsByName) do
				if context.playtestJobId == jobId and context.role == "server" and context.connection.Connected then
					serverContext = context
					break
				end
			end
			local function leaveFromEdit()
				local canLeave, allowed = pcall(function()
					return service:CanLeaveTest()
				end)
				local canLeaveError = nil
				if not canLeave then
					canLeaveError = "CanLeaveTest failed: " .. tostring(allowed)
				elseif allowed ~= true then
					canLeaveError = "Studio reported that it cannot leave this playtest"
				end
				if canLeaveError and not forceStop then
					return false, canLeaveError
				end
				local ok, result = pcall(function()
					return service:LeaveTest()
				end)
				if not ok then
					local detail = "LeaveTest failed: " .. tostring(result)
					if canLeaveError then
						detail = canLeaveError .. "; " .. detail
					end
					return false, detail
				end
				return true, nil
			end
			local function waitForEnd(seconds)
				local deadline = os.clock() + seconds
				while tostring(job.status) == "stopping" and os.clock() < deadline do
					task.wait(0.05)
				end
				return tostring(job.status) ~= "stopping"
			end
			local function endTestFallback(previousError)
				if not forceStop then
					return false, previousError
				end
				local ended, endResult = pcall(function()
					return service:EndTest(args.result or "Ro Sync forced playtest cleanup")
				end)
				local endError = nil
				if ended then
					if waitForEnd(3) then
						return true, nil
					end
					endError = "Studio accepted EndTest but the playtest remained active"
				else
					endError = "EndTest failed: " .. tostring(endResult)
				end
				if previousError and previousError ~= "" then
					return false, tostring(previousError) .. "; " .. endError
				end
				return false, endError
			end

			if forceStop then
				local left, leaveError = leaveFromEdit()
				if left then
					if waitForEnd(3) then
						return { stopped = true, job = playtestJobSnapshot(job), via = "StudioTestService.LeaveTest" }
					end
					leaveError = "Studio accepted LeaveTest but the playtest remained active"
				end
				if not serverContext then
					local ended, endError = endTestFallback(leaveError)
					if ended then
						return { stopped = true, job = playtestJobSnapshot(job), via = "StudioTestService.EndTest" }
					end
					failStop("unable to force-stop playtest: " .. tostring(endError))
				end
			end

			if serverContext then
				local requested, requestError = pcall(function()
					requestRuntimeContext(
						serverContext,
						"end_test",
						{ result = args.result },
						if forceStop then 1 else 5
					)
				end)
				if requested then
					waitForEnd(if forceStop then 3 else 10)
					if tostring(job.status) == "stopped" then
						return { stopped = true, job = playtestJobSnapshot(job), via = "PlayServer.EndTest" }
					end
					if tostring(job.status) ~= "stopping" then
						return { stopped = true, job = playtestJobSnapshot(job), via = "PlayServer.EndTest" }
					end
				end
				local left, leaveError = leaveFromEdit()
				if left then
					if waitForEnd(3) then
						return { stopped = true, job = playtestJobSnapshot(job), via = "StudioTestService.LeaveTest" }
					end
					leaveError = "Studio accepted LeaveTest but the playtest remained active"
				end
				local endError = leaveError
				if forceStop then
					local ended, fallbackError = endTestFallback(leaveError)
					if ended then
						return { stopped = true, job = playtestJobSnapshot(job), via = "StudioTestService.EndTest" }
					end
					endError = fallbackError
				end
				failStop(
					"unable to stop playtest through PlayServer ("
						.. tostring(requestError)
						.. ") or edit mode ("
						.. tostring(endError)
						.. ")"
				)
			end

			local left, leaveError = leaveFromEdit()
			if not left then
				local ended, endError = endTestFallback(leaveError)
				if ended then
					return { stopped = true, job = playtestJobSnapshot(job), via = "StudioTestService.EndTest" }
				end
				failStop("unable to stop playtest: " .. tostring(endError))
			end
			if not waitForEnd(3) then
				local ended, endError = endTestFallback("Studio accepted LeaveTest but the playtest remained active")
				if ended then
					return { stopped = true, job = playtestJobSnapshot(job), via = "StudioTestService.EndTest" }
				end
				failStop(tostring(endError))
			end
			return { stopped = true, job = playtestJobSnapshot(job) }
		end

		local function completionExitCode(outcome)
			if outcome == "success" then
				return 0
			elseif outcome == "failure" then
				return 2
			elseif outcome == "timeout" then
				return 3
			elseif outcome == "aborted" then
				return 4
			end
			return 5
		end

		local function auditPlayscriptCompletion(job, terminal)
			local postAudit = remoteState.postRemoteWriteLogSync
			if not postAudit then
				return false, "synchronous writelog transport is unavailable"
			end
			local run = job.run
			return postAudit({
				ts = os.time(),
				op = "playtest_run_complete",
				ok = terminal.outcome == "success",
				jobId = job.id,
				clientRunId = job.clientRunId,
				outcome = terminal.outcome,
				exitCode = completionExitCode(terminal.outcome),
				jobStatus = terminal.jobStatus,
				elapsed = terminal.elapsed,
				script = if run and run._main
					then { path = run._main.path, sha256 = run._main.sha256, bytes = #run._main.source }
					else nil,
				clientScript = if run and run._client
					then { path = run._client.path, sha256 = run._client.sha256, bytes = #run._client.source }
					else nil,
			})
		end

		local function cancelOpenPlayscriptRunners(job, reason)
			playscriptCoordinator:cancelRunners(job, runtimeContextsByName, reason)
			local failures = {}
			for _, context in pairs(runtimeContextsByName) do
				if context.playtestJobId == job.id and context.connection.Connected then
					local ok, result = pcall(function()
						return requestRuntimeContext(context, "playscript_cancel", { reason = reason }, 2)
					end)
					if not ok or type(result) ~= "table" or result.cancelled ~= true then
						local failure = if ok then "runtime did not confirm cancellation" else tostring(result)
						table.insert(failures, context.name .. ": " .. failure)
					end
				end
			end
			return if #failures > 0 then table.concat(failures, "; ") else nil
		end

		remoteState.onPlayscriptTerminalRequested = function(job, terminal, external)
			if external then
				terminal.jobStatus = job.status
				local auditOk, auditError = auditPlayscriptCompletion(job, terminal)
				if not auditOk then
					terminal.auditError = tostring(auditError)
				end
				playscriptCoordinator:finalizeTerminal(job, terminal)
				return
			end
			local run = job.run
			if not run or run.terminal then
				return
			end
			if run.keepOpen and terminal.outcome ~= "aborted" and terminal.force ~= true then
				local cancelError = cancelOpenPlayscriptRunners(job, terminal.outcome)
				local finalTerminal = terminal
				if cancelError then
					finalTerminal = table.clone(terminal)
					finalTerminal.originalOutcome = terminal.outcome
					finalTerminal.outcome = "aborted"
					finalTerminal.kind = "aborted"
					finalTerminal.ok = false
					finalTerminal.error = "playscript cleanup could not be confirmed: " .. cancelError
					finalTerminal.reason = finalTerminal.error
				end
				finalTerminal.jobStatus = job.status
				local auditOk, auditError = auditPlayscriptCompletion(job, finalTerminal)
				if not auditOk then
					finalTerminal.auditError = tostring(auditError)
				end
				playscriptCoordinator:finalizeTerminal(job, finalTerminal, cancelError)
				return
			end
			local forceStop = terminal.force == true
				or terminal.outcome == "timeout"
				or terminal.outcome == "bootFailure"
			local stopped, stopResult = pcall(function()
				return stopPlaytestJob(
					job,
					{ reason = terminal.outcome, result = "Ro Sync playscript completed" },
					forceStop
				)
			end)
			local stopConfirmed = stopped and type(stopResult) == "table" and stopResult.stopped == true
			local finalTerminal = terminal
			local stopError = nil
			if not stopConfirmed then
				stopError = if stopped then "playtest stop returned without confirmation" else tostring(stopResult)
				finalTerminal = table.clone(terminal)
				finalTerminal.originalOutcome = terminal.outcome
				finalTerminal.outcome = "aborted"
				finalTerminal.kind = "aborted"
				finalTerminal.ok = false
				finalTerminal.error = "playtest teardown could not be confirmed: " .. stopError
				finalTerminal.reason = finalTerminal.error
			end
			finalTerminal.jobStatus = job.status
			local auditOk, auditError = auditPlayscriptCompletion(job, finalTerminal)
			if not auditOk then
				finalTerminal.auditError = tostring(auditError)
			end
			playscriptCoordinator:finalizeTerminal(job, finalTerminal, stopError)
		end

		remoteHandlers.playtest_stop = function(args)
			local jobId = tostring(args.jobId or latestPlaytestJobId or "")
			if jobId == "" then
				error("no playtest job is active")
			end
			local job = playtestJobs[jobId]
			if not job then
				error("playtest job not found: " .. jobId)
			end
			if latestPlaytestJobId ~= jobId then
				error("refusing to stop a stale playtest generation: " .. jobId)
			end
			if tostring(job.status) ~= "starting" and tostring(job.status) ~= "running" then
				error("playtest job is not active: " .. tostring(job.status))
			end
			return stopPlaytestJob(job, args, args.force == true)
		end

		remoteHandlers.playtest_run_cancel = function(args)
			local jobId, job, clientRunId = playscriptJobForArgs(args)
			if jobId == "" and not job and clientRunId ~= "" then
				prunePlayscriptRunKeys()
				playscriptRunTombstones[clientRunId] = playscriptRunTombstones[clientRunId]
					or { createdAt = os.clock() }
				prunePlayscriptRunKeys()
				local terminal = {
					outcome = "aborted",
					kind = "aborted",
					ok = false,
					error = tostring(args.reason or "playtest run cancelled before launch"),
					reason = tostring(args.reason or "playtest run cancelled before launch"),
					jobStatus = "not-started",
					elapsed = 0,
				}
				return {
					cancelled = true,
					cleanupConfirmed = true,
					pendingStart = true,
					clientRunId = clientRunId,
					outcome = terminal,
				}
			end
			if jobId == "" or not job or not job.run or not playscriptCoordinator then
				error("playtest run not found: " .. jobId)
			end
			local cancelOutcome = job.run.terminal
			local forcedCleanupConfirmed = nil
			if cancelOutcome and args.force == true then
				local active = job.status == "starting" or job.status == "running" or job.status == "stopping"
				if active then
					local stopped, stopResult = pcall(function()
						return stopPlaytestJob(job, { reason = args.reason or "playtest run cleanup" }, true)
					end)
					forcedCleanupConfirmed = stopped and type(stopResult) == "table" and stopResult.stopped == true
					cancelOutcome = table.clone(cancelOutcome)
					cancelOutcome.jobStatus = job.status
					if forcedCleanupConfirmed then
						cancelOutcome.stopError = nil
					else
						local stopError = if stopped
							then "playtest stop returned without confirmation"
							else tostring(stopResult)
						cancelOutcome.originalOutcome = cancelOutcome.outcome
						cancelOutcome.outcome = "aborted"
						cancelOutcome.kind = "aborted"
						cancelOutcome.ok = false
						cancelOutcome.error = "playtest teardown could not be confirmed: " .. stopError
						cancelOutcome.reason = cancelOutcome.error
						cancelOutcome.stopError = stopError
					end
				end
			end
			if not job.run.terminal and not job.run._pendingTerminal then
				local reason = tostring(args.reason or "playtest run cancelled")
				local requestedOutcome = tostring(args.outcome or ""):lower()
				local outcome = if requestedOutcome == "timeout" or reason:lower():find("timeout", 1, true)
					then "timeout"
					else "aborted"
				playscriptCoordinator:requestTerminal(job, {
					outcome = outcome,
					kind = outcome,
					error = reason,
					reason = reason,
					jobStatus = job.status,
					force = args.force == true,
				})
			end
			local deadline = os.clock() + 20
			while not job.run.terminal and os.clock() < deadline do
				task.wait(0.05)
			end
			if not job.run.terminal then
				error("playtest run cancellation did not finish within 20 seconds")
			end
			cancelOutcome = cancelOutcome or job.run.terminal
			local cleanupConfirmed = if forcedCleanupConfirmed ~= nil
				then forcedCleanupConfirmed
				else cancelOutcome.stopError == nil
			return {
				cancelled = cleanupConfirmed,
				cleanupConfirmed = cleanupConfirmed,
				job = playtestJobSnapshot(job),
				run = playscriptCoordinator:snapshot(job),
				outcome = cancelOutcome,
				clientRunId = job.clientRunId,
			}
		end
	end

	do
		-- Broad read-only property sweep. pcall'd per-property so unsupported names on
		-- this class just get skipped. Not exhaustive — covers what an LLM is likely to
		-- ask about when inspecting an instance. `rosync get --prop <name>` still
		-- reaches any property directly.
		local COMMON_PROPS = {
			"Name",
			"ClassName",
			-- BasePart
			"Position",
			"Size",
			"Orientation",
			"Rotation",
			"CFrame",
			"Anchored",
			"CanCollide",
			"CanTouch",
			"CanQuery",
			"CastShadow",
			"Massless",
			"Transparency",
			"Reflectance",
			"Material",
			"Color",
			"BrickColor",
			"TopSurface",
			"BottomSurface",
			"CollisionGroup",
			"Shape",
			-- MeshPart / SpecialMesh
			"MeshId",
			"TextureContent",
			"TextureID",
			"MeshScale",
			"Scale",
			"MeshType",
			"VertexColor",
			-- SpawnLocation
			"Enabled",
			"Neutral",
			"AllowTeamChangeOnTouch",
			"Duration",
			"TeamColor",
			-- Script / LocalScript / ModuleScript
			"Source",
			"Disabled",
			"RunContext",
			-- GuiObject
			"AnchorPoint",
			"BackgroundColor3",
			"BackgroundTransparency",
			"BorderColor3",
			"BorderSizePixel",
			"ZIndex",
			"Visible",
			"LayoutOrder",
			"Active",
			"AutomaticSize",
			-- TextLabel / TextButton / TextBox
			"Text",
			"TextColor3",
			"TextSize",
			"TextScaled",
			"Font",
			"FontFace",
			"TextXAlignment",
			"TextYAlignment",
			"RichText",
			"TextWrapped",
			"TextStrokeColor3",
			"TextStrokeTransparency",
			"LineHeight",
			-- ImageLabel / ImageButton
			"Image",
			"ImageColor3",
			"ImageTransparency",
			"ScaleType",
			"ImageRectSize",
			"ImageRectOffset",
			"SliceCenter",
			"SliceScale",
			-- Attachment
			"Axis",
			"SecondaryAxis",
			"WorldPosition",
			"WorldCFrame",
			-- Beam / Trail
			"Attachment0",
			"Attachment1",
			"Width0",
			"Width1",
			"Texture",
			"TextureLength",
			"TextureSpeed",
			"TextureMode",
			"LightEmission",
			"LightInfluence",
			"FaceCamera",
			"Segments",
			-- ParticleEmitter
			"Rate",
			"Lifetime",
			"Speed",
			"SpreadAngle",
			"Rotation",
			"RotSpeed",
			"EmissionDirection",
			"Acceleration",
			"Drag",
			-- Camera
			"Focus",
			"FieldOfView",
			"CameraType",
			"CameraSubject",
			"HeadScale",
			-- Light
			"Brightness",
			"Range",
			"Shadows",
			"Angle",
			"Face",
			-- Sound
			"SoundId",
			"Volume",
			"Pitch",
			"PlaybackSpeed",
			"Looped",
			"Playing",
			"TimePosition",
			"PlayOnRemove",
			"RollOffMode",
			"RollOffMaxDistance",
			-- Decal / Texture
			"Face",
			"StudsPerTileU",
			"StudsPerTileV",
			-- Instance meta
			"Archivable",
			"Parent",
		}

		-- Extra property names used when inst:GetProperties() isn't exposed by this
		-- Studio build. Combined with COMMON_PROPS inside buildInspectPropsList —
		-- pcall-guarded, so unknown props on the target class are skipped.
		local BROAD_INSPECT_PROPS = {
			-- BasePart physics / misc
			"Locked",
			"Velocity",
			"RotVelocity",
			"AssemblyLinearVelocity",
			"AssemblyAngularVelocity",
			"AssemblyMass",
			"PivotOffset",
			"WorldPivot",
			"RootPriority",
			"CustomPhysicalProperties",
			"CollisionGroupId",
			"LocalTransparencyModifier",
			"ReceiveAge",
			"NetworkOwnershipAuto",
			-- DataModel / services
			"HttpEnabled",
			"LoadStringEnabled",
			"StreamingEnabled",
			"StreamingTargetRadius",
			"Gravity",
			"FallenPartsDestroyHeight",
			-- Lighting
			"Ambient",
			"OutdoorAmbient",
			"GlobalShadows",
			"GeographicLatitude",
			"ClockTime",
			"TimeOfDay",
			"FogStart",
			"FogEnd",
			"FogColor",
			"EnvironmentSpecularScale",
			"EnvironmentDiffuseScale",
			"Technology",
			-- Model
			"PrimaryPart",
			"ModelStreamingMode",
			"LevelOfDetail",
			"ScaleTo",
			-- Gui containers
			"AutoLocalize",
			"RootLocalizationTable",
			"ClipsDescendants",
			"SelectionImageObject",
			"SelectionOrder",
			"NextSelectionLeft",
			"NextSelectionRight",
			"NextSelectionUp",
			"NextSelectionDown",
			"SizeConstraint",
			-- ScrollingFrame
			"CanvasSize",
			"CanvasPosition",
			"AutomaticCanvasSize",
			"ScrollingDirection",
			"ScrollBarThickness",
			"ScrollBarImageColor3",
			"ScrollBarImageTransparency",
			"BottomImage",
			"MidImage",
			"TopImage",
			"ElasticBehavior",
			"ScrollingEnabled",
			"VerticalScrollBarInset",
			"HorizontalScrollBarInset",
			"VerticalScrollBarPosition",
			-- TextBox
			"PlaceholderText",
			"PlaceholderColor3",
			"ClearTextOnFocus",
			"MultiLine",
			"TextEditable",
			"CursorPosition",
			"SelectionStart",
			-- Buttons
			"AutoButtonColor",
			"HoverColor3",
			"PressedColor3",
			-- Sound / ParticleEmitter extras
			"MaxDistance",
			"EmitterSize",
			"DistanceFactor",
			"DopplerScale",
			"RollOffGain",
			"AmbientReverb",
			"TimeScale",
			"PlaybackRegion",
			"PlaybackRegionsEnabled",
			"ZOffset",
			"LockedToPart",
			-- UIScale / text
			"UIScale",
			"MaxVisibleGraphemes",
			-- Generic holders
			"Value",
		}

		-- Pick the property-name list used to populate `inspectInstance`.
		-- Prefers `inst:GetProperties()` when this Studio build exposes it (yields
		-- the exact list valid for this class); otherwise falls back to a broader
		-- pcall sweep built from COMMON_PROPS + BROAD_INSPECT_PROPS, deduped,
		-- original order preserved.
		local function buildInspectPropsList(inst)
			local ok, raw = pcall(function()
				return inst:GetProperties()
			end)
			if ok and type(raw) == "table" and #raw > 0 then
				local names = {}
				for _, entry in ipairs(raw) do
					local name
					if type(entry) == "string" then
						name = entry
					elseif type(entry) == "table" then
						name = entry.Name or entry.name
					end
					if type(name) == "string" and name ~= "" then
						table.insert(names, name)
					end
				end
				if #names > 0 then
					return names
				end
			end
			local seen = {}
			local merged = {}
			for _, list in ipairs({ COMMON_PROPS, BROAD_INSPECT_PROPS }) do
				for _, name in ipairs(list) do
					if not seen[name] then
						seen[name] = true
						table.insert(merged, name)
					end
				end
			end
			return merged
		end

		local function inspectInstance(inst)
			local props = {}
			for _, name in ipairs(buildInspectPropsList(inst)) do
				local ok, raw = pcall(function()
					if name == "Source" and inst:IsA("LuaSourceContainer") then
						return readScriptSource(inst)
					end
					return inst[name]
				end)
				if ok and raw ~= nil then
					local eok, encoded = pcall(encodeRemoteValue, raw)
					if eok then
						props[name] = encoded
					end
				end
			end
			local attrs = {}
			local aok, rawAttrs = pcall(function()
				return inst:GetAttributes()
			end)
			if aok and typeof(rawAttrs) == "table" then
				for k, v in pairs(rawAttrs) do
					local eok, encoded = pcall(encodeRemoteValue, v)
					if eok then
						attrs[tostring(k)] = encoded
					end
				end
			end
			local tags = {}
			local tok, rawTags = pcall(function()
				return game:GetService("CollectionService"):GetTags(inst)
			end)
			if tok and typeof(rawTags) == "table" then
				for _, t in ipairs(rawTags) do
					table.insert(tags, tostring(t))
				end
			end
			local kids = {}
			for _, child in ipairs(inst:GetChildren()) do
				table.insert(kids, { class = child.ClassName, name = child.Name })
			end
			return {
				class = inst.ClassName,
				name = inst.Name,
				path = instancePathString(inst),
				properties = props,
				attributes = attrs,
				tags = tags,
				children = kids,
			}
		end

		remoteHandlers.get = function(args)
			local inst = resolveRemotePath(tostring(args.path or ""))
			if not inst then
				error("path not found: " .. tostring(args.path))
			end
			local prop = tostring(args.prop or "")
			if prop == "" then
				return inspectInstance(inst)
			end
			if prop == "Source" and inst:IsA("LuaSourceContainer") then
				return encodeRemoteValue(readScriptSource(inst))
			end
			return encodeRemoteValue(inst[prop])
		end

		local function canonicalEtagValue(value: any, depth: number?, seen: any?): string
			local currentDepth = depth or 0
			if currentDepth > 72 then
				return "<max-depth>"
			end
			local valueType = type(value)
			if valueType == "nil" then
				return "n"
			elseif valueType == "boolean" then
				return if value then "b1" else "b0"
			elseif valueType == "number" then
				return "d" .. tostring(value)
			elseif valueType == "string" then
				return "s" .. tostring(#value) .. ":" .. value
			elseif valueType ~= "table" then
				return valueType .. ":" .. tostring(value)
			end

			local visited = seen or {}
			if visited[value] then
				return "<cycle>"
			end
			visited[value] = true
			local keys = {}
			for key in pairs(value) do
				table.insert(keys, { key = key, order = type(key) .. ":" .. tostring(key) })
			end
			table.sort(keys, function(a, b)
				return a.order < b.order
			end)
			local parts = { "{" }
			for _, entry in ipairs(keys) do
				table.insert(parts, canonicalEtagValue(entry.key, currentDepth + 1, visited))
				table.insert(parts, "=")
				table.insert(parts, canonicalEtagValue(value[entry.key], currentDepth + 1, visited))
				table.insert(parts, ";")
			end
			table.insert(parts, "}")
			visited[value] = nil
			return table.concat(parts)
		end

		local function instanceEtag(inst: Instance): string
			local identity = ""
			pcall(function()
				identity = inst:GetDebugId(32)
			end)
			if identity == "" then
				identity = instancePathString(inst) .. "\0" .. inst.ClassName
			end
			-- The identity alone only detects replacement at a path. Include a stable,
			-- canonical snapshot so an etag precondition also rejects intervening
			-- property, attribute, tag, name, parent, or direct-child changes.
			local state = inspectInstance(inst)
			return sha256Hex(identity .. "\0" .. canonicalEtagValue(state))
		end

		remoteState.instanceEtag = instanceEtag

		remoteHandlers.inspect_ref = function(args)
			local path = tostring(args.path or "")
			local inst = resolveRemotePath(path)
			if not inst then
				error("path not found: " .. path)
			end
			local expectedClass = tostring(args.expectedClass or "")
			if expectedClass ~= "" and inst.ClassName ~= expectedClass then
				error(
					"class precondition failed at "
						.. path
						.. ": expected "
						.. expectedClass
						.. ", found "
						.. inst.ClassName
				)
			end
			local etag = instanceEtag(inst)
			local expectedEtag = tostring(args.etag or "")
			if expectedEtag ~= "" and etag ~= expectedEtag then
				error("etag precondition failed at " .. path .. ": expected " .. expectedEtag .. ", found " .. etag)
			end
			local result = {
				path = instancePathString(inst),
				class = inst.ClassName,
				name = inst.Name,
				etag = etag,
			}
			local prop = tostring(args.prop or args.property or "")
			if prop ~= "" then
				result.prop = prop
				if prop == "Source" and inst:IsA("LuaSourceContainer") then
					result.value = encodeRemoteValue(readScriptSource(inst))
				else
					result.value = encodeRemoteValue(inst[prop])
				end
			end
			if type(args.props) == "table" then
				if #args.props > 64 then
					error("inspect_ref supports at most 64 properties")
				end
				result.values = {}
				for _, rawProp in ipairs(args.props) do
					local requestedProp = tostring(rawProp)
					if requestedProp == "" or #requestedProp > 128 then
						error("inspect_ref property names must contain 1-128 bytes")
					end
					if requestedProp == "Source" and inst:IsA("LuaSourceContainer") then
						result.values[requestedProp] = encodeRemoteValue(readScriptSource(inst))
					else
						result.values[requestedProp] = encodeRemoteValue(inst[requestedProp])
					end
				end
			end
			return result
		end

		local activeWorkflowRecordings = {}
		local WORKFLOW_RECORDING_ATTRIBUTE = "RoSyncActiveWorkflowRecording"

		-- Studio recordings survive a plugin reload. Recover the identifier before
		-- accepting any new workflow so an interrupted prior transaction cannot leave
		-- ChangeHistoryService permanently occupied.
		local staleRecording = nil
		pcall(function()
			staleRecording = plugin:GetAttribute(WORKFLOW_RECORDING_ATTRIBUTE)
		end)
		if type(staleRecording) == "string" and staleRecording ~= "" then
			pcall(function()
				ChangeHistoryService:FinishRecording(staleRecording, Enum.FinishRecordingOperation.Cancel)
			end)
			pcall(function()
				plugin:SetAttribute(WORKFLOW_RECORDING_ATTRIBUTE, nil)
			end)
		end

		local function cancelActiveWorkflowRecordings()
			local cancelledCount = 0
			for transactionId, recordingId in pairs(activeWorkflowRecordings) do
				local ok = pcall(function()
					ChangeHistoryService:FinishRecording(recordingId, Enum.FinishRecordingOperation.Cancel)
				end)
				if ok then
					activeWorkflowRecordings[transactionId] = nil
					cancelledCount += 1
				end
			end
			if next(activeWorkflowRecordings) == nil then
				pcall(function()
					plugin:SetAttribute(WORKFLOW_RECORDING_ATTRIBUTE, nil)
				end)
			end
			local remainingCount = 0
			for _ in pairs(activeWorkflowRecordings) do
				remainingCount += 1
			end
			return cancelledCount, remainingCount
		end

		remoteState.cancelActiveWorkflowRecordings = cancelActiveWorkflowRecordings

		remoteHandlers.transaction_begin = function(args)
			local transactionId = tostring(args.id or "")
			if transactionId == "" then
				error("missing transaction id")
			end
			if activeWorkflowRecordings[transactionId] then
				error("transaction already active: " .. transactionId)
			end
			for activeId in pairs(activeWorkflowRecordings) do
				error("transaction already active: " .. tostring(activeId))
			end
			local name = tostring(args.name or ("Ro Sync workflow " .. transactionId))
			local recordingId = ChangeHistoryService:TryBeginRecording(name, name)
			if recordingId == nil then
				error("Studio refused to begin change-history recording")
			end
			activeWorkflowRecordings[transactionId] = recordingId
			local stored, storeError = pcall(function()
				plugin:SetAttribute(WORKFLOW_RECORDING_ATTRIBUTE, recordingId)
			end)
			if not stored then
				pcall(function()
					ChangeHistoryService:FinishRecording(recordingId, Enum.FinishRecordingOperation.Cancel)
				end)
				activeWorkflowRecordings[transactionId] = nil
				error("failed to persist transaction recovery state: " .. tostring(storeError))
			end
			return { id = transactionId, recordingId = recordingId, active = true }
		end

		remoteHandlers.transaction_finish = function(args)
			local transactionId = tostring(args.id or "")
			local action = tostring(args.action or "commit"):lower()
			local recordingId = activeWorkflowRecordings[transactionId]
			if recordingId == nil then
				-- Cancellation is deliberately idempotent. The ordered write lane may
				-- already have auto-canceled a timed-out transaction before a queued
				-- explicit cancel reaches this handler.
				if action == "cancel" then
					return { id = transactionId, action = action, active = false, alreadyFinished = true }
				end
				error("transaction is not active: " .. transactionId)
			end
			local operation
			if action == "commit" then
				operation = Enum.FinishRecordingOperation.Commit
			elseif action == "cancel" then
				operation = Enum.FinishRecordingOperation.Cancel
			elseif action == "append" then
				operation = Enum.FinishRecordingOperation.Append
			else
				error("transaction action must be commit, cancel, or append")
			end
			ChangeHistoryService:FinishRecording(recordingId, operation)
			activeWorkflowRecordings[transactionId] = nil
			pcall(function()
				plugin:SetAttribute(WORKFLOW_RECORDING_ATTRIBUTE, nil)
			end)
			return { id = transactionId, action = action, active = false }
		end

		remoteHandlers.set = function(args)
			local inst = resolveRemotePath(tostring(args.path or ""))
			if not inst then
				error("path not found: " .. tostring(args.path))
			end
			local prop = tostring(args.prop or "")
			if prop == "" then
				error("missing prop")
			end
			local decoded = decodeRemoteValue(args.value)
			-- Route script Source through ScriptEditorService so Studio's open editor
			-- sees the new text, matching applyProperties behavior for daemon-driven ops.
			if prop == "Source" and inst:IsA("LuaSourceContainer") then
				if not writeScriptSource(inst, tostring(decoded or ""), false) then
					error("source write was not applied for " .. instancePathString(inst))
				end
			else
				inst[prop] = decoded
			end
			return { path = instancePathString(inst), prop = prop, etag = instanceEtag(inst) }
		end

		remoteHandlers.ls = function(args)
			local inst = resolveRemotePath(tostring(args.path or ""))
			if not inst then
				error("path not found: " .. tostring(args.path))
			end
			local out = {}
			for _, child in ipairs(inst:GetChildren()) do
				table.insert(out, {
					class = child.ClassName,
					name = child.Name,
					path = instancePathString(child),
				})
			end
			return out
		end

		local function buildRemoteTree(inst, remaining)
			local node = { class = inst.ClassName, name = inst.Name, children = {} }
			if remaining <= 0 then
				if #inst:GetChildren() > 0 then
					node.truncated = true
				end
				return node
			end
			for _, child in ipairs(inst:GetChildren()) do
				table.insert(node.children, buildRemoteTree(child, remaining - 1))
			end
			return node
		end

		remoteHandlers.tree = function(args)
			local inst = resolveRemotePath(tostring(args.path or ""))
			if not inst then
				error("path not found: " .. tostring(args.path))
			end
			local depth = tonumber(args.depth) or TREE_DEPTH_DEFAULT
			if depth < 0 then
				depth = 0
			end
			return buildRemoteTree(inst, math.floor(depth))
		end

		remoteHandlers.query = function(args)
			local selector = tostring(args.selector or "")
			if selector == "" then
				error("missing selector")
			end
			if #selector > 4096 then
				error("selector exceeds 4096 bytes")
			end
			local pattern = splitRemotePath(selector)
			if #pattern > 128 then
				error("selector exceeds 128 path segments")
			end
			local limit = math.clamp(math.floor(tonumber(args.limit) or 5000), 1, 10000)
			local requestedProps = {}
			if type(args.props) == "table" then
				for _, rawProp in ipairs(args.props) do
					if #requestedProps >= 32 then
						error("query supports at most 32 projected properties")
					end
					local prop = tostring(rawProp)
					if prop == "" or #prop > 128 then
						error("query property names must contain 1-128 bytes")
					end
					table.insert(requestedProps, prop)
				end
			end
			local includeAttributes = args.attributes == true
			local includeTags = args.tags == true
			local collectionService = game:GetService("CollectionService")
			local matches = {}
			local truncated = false
			local truncationReason = nil
			local nodeBudget = math.clamp(math.floor(tonumber(args.nodeBudget) or 100000), 1, 200000)
			local timeBudget = math.clamp(tonumber(args.timeoutSeconds) or 2, 0.05, 5)
			local deadline = os.clock() + timeBudget
			local responseBytes = 0
			local responseBudget = 4 * 1024 * 1024

			local function pathMatches(path, pathIndex, patternIndex)
				local memo = {}
				local function visit(currentPathIndex, currentPatternIndex)
					local key = tostring(currentPathIndex) .. ":" .. tostring(currentPatternIndex)
					if memo[key] ~= nil then
						return memo[key]
					end
					local result
					if currentPatternIndex > #pattern then
						result = currentPathIndex > #path
					else
						local token = pattern[currentPatternIndex]
						if token == "**" then
							result = visit(currentPathIndex, currentPatternIndex + 1)
								or (currentPathIndex <= #path and visit(currentPathIndex + 1, currentPatternIndex))
						elseif currentPathIndex <= #path and (token == "*" or token == path[currentPathIndex]) then
							result = visit(currentPathIndex + 1, currentPatternIndex + 1)
						else
							result = false
						end
					end
					memo[key] = result
					return result
				end
				return visit(pathIndex, patternIndex)
			end

			local function appendMatch(inst, segments)
				local item = {
					path = table.concat(segments, "/"),
					class = inst.ClassName,
					name = inst.Name,
					childrenCount = #inst:GetChildren(),
				}
				local encodeState = { nodes = 0, stringBytes = 0, seen = {} }
				if #requestedProps > 0 then
					item.properties = {}
					for _, prop in ipairs(requestedProps) do
						local ok, value = pcall(function()
							return inst[prop]
						end)
						if ok then
							local encodeOk, encoded = pcall(encodeRemoteValue, value, 0, encodeState)
							if encodeOk then
								item.properties[prop] = encoded
							end
						end
					end
				end
				if includeAttributes then
					item.attributes = encodeRemoteValue(inst:GetAttributes(), 0, encodeState)
				end
				if includeTags then
					item.tags = collectionService:GetTags(inst)
					table.sort(item.tags)
				end
				local okSize, encoded = pcall(function()
					return HttpService:JSONEncode(item)
				end)
				local itemBytes = if okSize then #encoded else 1024
				if responseBytes + itemBytes > responseBudget then
					truncated = true
					truncationReason = "response-bytes"
					return false
				end
				responseBytes += itemBytes
				table.insert(matches, item)
				return true
			end

			local stack = {}
			local services = game:GetChildren()
			for index = #services, 1, -1 do
				local service = services[index]
				table.insert(stack, { inst = service, segments = { service.Name } })
			end
			local visitedNodes = 0
			while #stack > 0 do
				if #matches >= limit then
					truncated = true
					truncationReason = "matches"
					break
				end
				if visitedNodes >= nodeBudget then
					truncated = true
					truncationReason = "nodes"
					break
				end
				if os.clock() >= deadline then
					truncated = true
					truncationReason = "time"
					break
				end
				local entry = table.remove(stack)
				visitedNodes += 1
				if pathMatches(entry.segments, 1, 1) and not appendMatch(entry.inst, entry.segments) then
					break
				end
				local children = entry.inst:GetChildren()
				for index = #children, 1, -1 do
					local child = children[index]
					local childSegments = table.clone(entry.segments)
					table.insert(childSegments, child.Name)
					table.insert(stack, { inst = child, segments = childSegments })
				end
			end
			return {
				matches = matches,
				count = #matches,
				truncated = truncated,
				truncationReason = truncationReason,
				limit = limit,
				visitedNodes = visitedNodes,
				responseBytes = responseBytes,
			}
		end

		remoteHandlers.find = function(args)
			-- Accept either `under` (new, scoped traversal) or `root` (legacy).
			-- Empty string resolves to the DataModel root.
			local rootPath = args.under or args.root or ""
			local root = resolveRemotePath(tostring(rootPath))
			if not root then
				error("root not found: " .. tostring(rootPath))
			end
			local className = args.className and tostring(args.className) or nil
			local nameFilter = args.name and tostring(args.name) or nil
			local limit = tonumber(args.limit) or FIND_LIMIT_DEFAULT
			local out = {}
			local function walk(inst)
				for _, c in ipairs(inst:GetChildren()) do
					if #out >= limit then
						return
					end
					local classOk = className == nil
					if not classOk then
						classOk = c.ClassName == className
						if not classOk then
							local ok, isA = pcall(function()
								return c:IsA(className)
							end)
							if ok and isA then
								classOk = true
							end
						end
					end
					local nameOk = nameFilter == nil or c.Name == nameFilter
					if classOk and nameOk then
						table.insert(out, instancePathString(c))
					end
					walk(c)
				end
			end
			walk(root)
			return out
		end

		remoteHandlers.eval = function(args)
			local src = tostring(args.source or "")
			if src == "" then
				error("missing source")
			end
			local env = {
				game = game,
				workspace = workspace,
				string = string,
				table = table,
				math = math,
				task = { wait = task.wait },
				-- Minimal extras so evals are actually usable; the spec's list
				-- (game/workspace/string/table/math/task.wait) is hostile on its own.
				print = print,
				tostring = tostring,
				tonumber = tonumber,
				type = type,
				typeof = typeof,
				ipairs = ipairs,
				pairs = pairs,
				next = next,
				select = select,
				pcall = pcall,
				error = error,
				assert = assert,
				unpack = unpack,
				Instance = Instance,
				Vector2 = Vector2,
				Vector3 = Vector3,
				CFrame = CFrame,
				Color3 = Color3,
				UDim = UDim,
				UDim2 = UDim2,
				Enum = Enum,
				BrickColor = BrickColor,
				NumberRange = NumberRange,
			}
			local fn, compileErr = loadstring(src, EVAL_CHUNK_NAME)
			if not fn then
				error("compile: " .. tostring(compileErr))
			end
			local setEnvOk, setEnvErr = pcall(function()
				setfenv(fn, env)
			end)
			if not setEnvOk then
				error("sandbox: " .. tostring(setEnvErr))
			end
			local results = table.pack(pcall(fn))
			local ok = results[1]
			if not ok then
				error("runtime: " .. tostring(results[2]))
			end
			local n = results.n - 1
			if n == 0 then
				return nil
			end
			if n == 1 then
				return encodeRemoteValue(results[2])
			end
			local enc = {}
			local encodeState = { nodes = 0, stringBytes = 0, seen = {} }
			for i = 1, n do
				enc[i] = encodeRemoteValue(results[i + 1], 0, encodeState)
			end
			return enc
		end
	end

	do
		local function encodeRemoteBuffer(buf)
			local encoded = EncodingService:Base64Encode(buf)
			if typeof(encoded) == "buffer" then
				return buffer.tostring(encoded)
			end
			return tostring(encoded)
		end

		remoteState.encodeRemoteBuffer = encodeRemoteBuffer

		local function runTransmitSource(src: string): any
			if src == "" then
				return nil
			end

			local function collectResults(results: { any })
				if not results[1] then
					error("runtime: " .. tostring(results[2]))
				end
				local n = (results :: any).n - 1
				if n == 0 then
					return nil
				end
				if n == 1 then
					return results[2]
				end
				local out = {}
				for i = 1, n do
					out[i] = results[i + 1]
				end
				return out
			end

			-- Run transmit scripts in the plugin's real Studio environment. This keeps
			-- globals such as game/workspace/require/Rect available without the old
			-- custom sandbox getting between render code and place-owned modules.
			local fn, compileErr = loadPluginChunk(src, "rosync-transmit")
			if not fn then
				error("compile: " .. tostring(compileErr))
			end
			return collectResults(table.pack(pcall(fn)))
		end

		local function isEmptyContentValue(value: any): boolean
			if value == nil then
				return true
			end
			if typeof(value) == "string" then
				return value == ""
			end

			local valueString = ""
			local ok = pcall(function()
				valueString = tostring(value)
			end)
			return ok and (valueString == "" or valueString == "Content()")
		end

		local function objectFromContentValue(content: any): any
			if typeof(content) == "string" then
				return nil
			end

			local object
			local ok = pcall(function()
				object = content.Object
			end)
			if ok and object ~= nil then
				return object
			end
			return nil
		end

		local function editableFromContentValue(content: any, context: string): any
			local object = objectFromContentValue(content)
			if object ~= nil then
				return object
			end

			local resolvedContent = content
			if typeof(resolvedContent) == "string" then
				resolvedContent = Content.fromUri(resolvedContent)
			end

			local editable
			local ok, err = pcall(function()
				editable = AssetService:CreateEditableImageAsync(resolvedContent)
			end)
			if ok and editable then
				return editable
			end
			error("CreateEditableImageAsync failed for " .. context .. ": " .. tostring(err))
		end

		local function editableFromImageGui(gui: Instance): any
			local okContent, content = pcall(function()
				return (gui :: any).ImageContent
			end)
			if okContent and content ~= nil then
				return editableFromContentValue(content, "ImageContent for " .. instancePathString(gui))
			end

			local image = ""
			pcall(function()
				image = tostring((gui :: any).Image or "")
			end)
			if image == "" then
				error("image gui has no Image/ImageContent: " .. instancePathString(gui))
			end
			return editableFromContentValue(image, "Image for " .. instancePathString(gui))
		end

		local function editableFromMeshPart(meshPart: Instance): any
			local okContent, content = pcall(function()
				return (meshPart :: any).TextureContent
			end)
			if okContent and not isEmptyContentValue(content) then
				return editableFromContentValue(content, "TextureContent for " .. instancePathString(meshPart))
			end

			local textureId = ""
			pcall(function()
				textureId = tostring((meshPart :: any).TextureID or "")
			end)
			if textureId == "" then
				return nil
			end
			return editableFromContentValue(textureId, "TextureID for " .. instancePathString(meshPart))
		end

		local resolveTransmitImage

		resolveTransmitImage = function(value: any, nameHint: string?, pathHint: string?)
			local target = value
			local isInstance = typeof(target) == "Instance"
			local editable = target
			if
				(isInstance and (target :: Instance):IsA("ImageLabel"))
				or (isInstance and (target :: Instance):IsA("ImageButton"))
			then
				editable = editableFromImageGui(target)
			elseif isInstance and (target :: Instance):IsA("MeshPart") then
				local meshEditable = editableFromMeshPart(target)
				if meshEditable == nil then
					return nil
				end
				editable = meshEditable
			else
				local object = objectFromContentValue(target)
				if object ~= nil then
					editable = object
				end
			end

			local targetName = nameHint
			if targetName == nil then
				targetName = if isInstance then (target :: Instance).Name else typeof(target)
			end

			local size
			local okSize = pcall(function()
				size = (editable :: any).Size
			end)
			if not okSize or typeof(size) ~= "Vector2" then
				return nil
			end

			local width = math.floor(size.X)
			local height = math.floor(size.Y)
			if width <= 0 or height <= 0 then
				error("invalid EditableImage size for " .. tostring(targetName))
			end

			return {
				editable = editable,
				name = targetName,
				path = pathHint or (if isInstance then instancePathString(target) else nil),
				class = if isInstance then (target :: Instance).ClassName else typeof(target),
				width = width,
				height = height,
			}
		end

		local transmitSessionCounter = 0
		local transmitSessions = {}

		local function appendTransmitPrepared(
			value: any,
			sessionImages: { any },
			out: { any },
			seen: { [any]: boolean },
			nameHint: string?,
			pathHint: string?
		)
			if value == nil then
				return
			end
			local t = typeof(value)
			if t == "table" then
				local image = value.image or value.capture or value.editableImage or value.editable
				if image ~= nil then
					appendTransmitPrepared(
						image,
						sessionImages,
						out,
						seen,
						value.name or value.Name or nameHint,
						value.path or pathHint
					)
					return
				end
				for key, item in pairs(value) do
					local childName = nameHint
					if type(key) == "string" then
						childName = key
					end
					appendTransmitPrepared(item, sessionImages, out, seen, childName, pathHint)
				end
				return
			end

			if seen[value] then
				return
			end
			seen[value] = true
			local image = resolveTransmitImage(value, nameHint, pathHint)
			if image == nil then
				return
			end

			local index = #sessionImages + 1
			sessionImages[index] = image
			table.insert(out, {
				token = tostring(index),
				name = image.name,
				path = image.path,
				class = image.class,
				width = image.width,
				height = image.height,
			})
		end

		local function appendTransmitPreparedPath(
			path: string,
			sessionImages: { any },
			out: { any },
			seen: { [any]: boolean }
		)
			local inst = resolveRemotePath(path)
			if not inst then
				error("path not found: " .. path)
			end
			appendTransmitPrepared(inst, sessionImages, out, seen, nil, instancePathString(inst))
		end

		local function appendTransmitPreparedFrom(
			path: string,
			sessionImages: { any },
			out: { any },
			seen: { [any]: boolean }
		)
			local root = resolveRemotePath(path)
			if not root then
				error("from path not found: " .. path)
			end
			appendTransmitPrepared(root, sessionImages, out, seen, nil, instancePathString(root))
			for _, inst in ipairs(root:GetDescendants()) do
				appendTransmitPrepared(inst, sessionImages, out, seen, nil, instancePathString(inst))
			end
		end

		remoteHandlers.transmit_prepare = function(args)
			local out = {}
			local seen = {}
			local sessionImages = {}

			local source = tostring(args.source or "")
			local result = runTransmitSource(source)
			appendTransmitPrepared(result, sessionImages, out, seen, nil, nil)

			local paths = args.paths
			if type(paths) == "table" then
				for _, path in ipairs(paths) do
					appendTransmitPreparedPath(tostring(path), sessionImages, out, seen)
				end
			end

			local fromPath = args.from
			if fromPath ~= nil and tostring(fromPath) ~= "" then
				appendTransmitPreparedFrom(tostring(fromPath), sessionImages, out, seen)
			end

			transmitSessionCounter += 1
			local sessionId = tostring(transmitSessionCounter)
			transmitSessions[sessionId] = sessionImages

			return {
				sessionId = sessionId,
				images = out,
			}
		end

		remoteHandlers.transmit_read = function(args)
			local sessionId = tostring(args.sessionId or "")
			local token = tostring(args.token or "")
			local sessionImages = transmitSessions[sessionId]
			if sessionImages == nil then
				error("transmit session not found: " .. sessionId)
			end

			local image = sessionImages[tonumber(token) or -1]
			if image == nil then
				error("transmit image token not found: " .. token)
			end

			local pixels
			local okPixels, err = pcall(function()
				pixels = (image.editable :: any):ReadPixelsBuffer(Vector2.zero, Vector2.new(image.width, image.height))
			end)
			if not okPixels or pixels == nil then
				error("ReadPixelsBuffer failed for " .. tostring(image.name) .. ": " .. tostring(err))
			end

			return {
				name = image.name,
				path = image.path,
				class = image.class,
				width = image.width,
				height = image.height,
				pixelsBase64 = encodeRemoteBuffer(pixels),
			}
		end

		remoteHandlers.transmit_close = function(args)
			local sessionId = tostring(args.sessionId or "")
			transmitSessions[sessionId] = nil
			return true
		end
	end

	do
		-- ---------------------------------------------------------------------------
		-- Locally packaged Photo capture. This never requires capture code from the
		-- open place. The module owns all temporary scene changes; this bridge validates the
		-- request and exposes the resulting RGBA buffer in bounded chunks.
		-- ---------------------------------------------------------------------------

		local PHOTO_SESSION_TTL_SECONDS = 120
		local PHOTO_MAX_CHUNK_BYTES = 512 * 1024
		local PHOTO_MAX_AXIS = 4096
		local PHOTO_MAX_PIXELS = 16 * 1024 * 1024
		local PHOTO_MAX_BYTES = PHOTO_MAX_PIXELS * 4
		local PHOTO_MAX_SESSIONS = 2
		local PHOTO_CFRAME_EPSILON = 1e-3
		local photoSessionCounter = 0
		local photoSessions = {}
		local photoPrepareBusy = false
		local photoModule = nil

		local function getPhotoModule()
			if photoModule ~= nil then
				return photoModule
			end
			local moduleScript = script.Parent:FindFirstChild("Photo")
			if moduleScript == nil or not moduleScript:IsA("ModuleScript") then
				error("Ro Sync Photo module is not packaged in this plugin; reinstall Ro Sync")
			end
			photoModule = require(moduleScript)
			return photoModule
		end

		local function finiteNumber(value: any, fieldName: string): number
			local number = tonumber(value)
			if number == nil or number ~= number or number == math.huge or number == -math.huge then
				error(fieldName .. " must be a finite number")
			end
			return number
		end

		local function photoSize(value: any, fieldName: string): Vector2?
			if value == nil then
				return nil
			end
			if type(value) ~= "table" then
				error(fieldName .. " must be an object with x and y")
			end
			local x = math.floor(finiteNumber(value.x, fieldName .. ".x"))
			local y = math.floor(finiteNumber(value.y, fieldName .. ".y"))
			if x < 1 or y < 1 then
				error(fieldName .. " dimensions must be positive")
			end
			if x > PHOTO_MAX_AXIS or y > PHOTO_MAX_AXIS or x * y > PHOTO_MAX_PIXELS then
				error(fieldName .. " exceeds the Ro Sync Photo size limit")
			end
			return Vector2.new(x, y)
		end

		local function photoRect(value: any): Rect?
			if value == nil then
				return nil
			end
			if type(value) ~= "table" then
				error("nativeRect must be an object with x, y, width, and height")
			end
			local x = math.floor(finiteNumber(value.x, "nativeRect.x"))
			local y = math.floor(finiteNumber(value.y, "nativeRect.y"))
			local width = math.floor(finiteNumber(value.width, "nativeRect.width"))
			local height = math.floor(finiteNumber(value.height, "nativeRect.height"))
			if x < 0 or y < 0 or width < 1 or height < 1 then
				error("nativeRect coordinates must be non-negative and dimensions must be positive")
			end
			if width > PHOTO_MAX_AXIS or height > PHOTO_MAX_AXIS or width * height > PHOTO_MAX_PIXELS then
				error("nativeRect exceeds the Ro Sync Photo size limit")
			end
			return Rect.new(x, y, x + width, y + height)
		end

		local function photoDirection(value: any): Vector3?
			if value == nil then
				return nil
			end
			if type(value) ~= "table" then
				error("direction must be an object with x, y, and z")
			end
			local direction = Vector3.new(
				finiteNumber(value.x, "direction.x"),
				finiteNumber(value.y, "direction.y"),
				finiteNumber(value.z, "direction.z")
			)
			if direction.Magnitude <= 1e-6 then
				error("direction cannot be the zero vector")
			end
			return direction.Unit
		end

		local function photoCameraCFrame(value: any): CFrame?
			if value == nil then
				return nil
			end
			if type(value) ~= "table" or value.__type ~= "CFrame" then
				error('cameraCFrame must be a tagged CFrame object with __type = "CFrame" and 12 components')
			end
			local supplied = value.components
			if type(supplied) ~= "table" or #supplied ~= 12 then
				error("cameraCFrame components must contain exactly 12 numbers")
			end

			local components = table.create(12)
			for index = 1, 12 do
				if type(supplied[index]) ~= "number" then
					error("cameraCFrame components must contain exactly 12 finite numbers")
				end
				components[index] = finiteNumber(supplied[index], "cameraCFrame.components[" .. tostring(index) .. "]")
			end

			-- CFrame's final nine components are a rigid rotation matrix. Validate
			-- both orthonormality and handedness before constructing it so malformed
			-- remote input cannot create a skewed, reflected, or degenerate camera.
			local row0 = Vector3.new(components[4], components[5], components[6])
			local row1 = Vector3.new(components[7], components[8], components[9])
			local row2 = Vector3.new(components[10], components[11], components[12])
			local determinant = row0:Dot(row1:Cross(row2))
			if
				math.abs(row0.Magnitude - 1) > PHOTO_CFRAME_EPSILON
				or math.abs(row1.Magnitude - 1) > PHOTO_CFRAME_EPSILON
				or math.abs(row2.Magnitude - 1) > PHOTO_CFRAME_EPSILON
				or math.abs(row0:Dot(row1)) > PHOTO_CFRAME_EPSILON
				or math.abs(row0:Dot(row2)) > PHOTO_CFRAME_EPSILON
				or math.abs(row1:Dot(row2)) > PHOTO_CFRAME_EPSILON
				or math.abs(determinant - 1) > PHOTO_CFRAME_EPSILON
			then
				error("cameraCFrame rotation must be a rigid right-handed matrix")
			end

			return CFrame.new(
				components[1],
				components[2],
				components[3],
				components[4],
				components[5],
				components[6],
				components[7],
				components[8],
				components[9],
				components[10],
				components[11],
				components[12]
			)
		end

		local function encodedPhotoCameraCFrame(value: CFrame?): any
			if value == nil then
				return nil
			end
			return {
				__type = "CFrame",
				components = { value:GetComponents() },
			}
		end

		local function photoUiTarget(value: any): Instance?
			if value == nil then
				return nil
			end
			if type(value) ~= "string" or value == "" then
				error("uiTarget must be a non-empty Studio instance path")
			end
			local target = resolveRemotePath(value)
			if target == nil then
				error("photo UI target path not found: " .. value)
			end

			local screenGui: ScreenGui?
			if target:IsA("ScreenGui") then
				screenGui = target
			elseif target:IsA("GuiObject") then
				local ancestor = target:FindFirstAncestorWhichIsA("ScreenGui")
				if ancestor ~= nil then
					screenGui = ancestor :: ScreenGui
				end
			else
				error("uiTarget must resolve to a ScreenGui or GuiObject")
			end
			if screenGui == nil then
				error("uiTarget GuiObject must be a descendant of a ScreenGui")
			end

			local starterGui = game:GetService("StarterGui")
			local coreGui = game:GetService("CoreGui")
			if not screenGui:IsDescendantOf(starterGui) and not screenGui:IsDescendantOf(coreGui) then
				error("uiTarget must belong to an edit-mode ScreenGui under StarterGui or CoreGui")
			end
			return target
		end

		local function photoRegionMetadata(value: any): any
			if value == nil then
				return nil
			end
			if typeof(value) ~= "Rect" then
				error("Photo capture region metadata must be a Rect")
			end
			return {
				x = value.Min.X,
				y = value.Min.Y,
				width = value.Width,
				height = value.Height,
			}
		end

		local function photoFullSizeMetadata(value: any): any
			if value == nil then
				return nil
			end
			if typeof(value) ~= "Vector2" then
				error("Photo capture fullSize metadata must be a Vector2")
			end
			return {
				x = value.X,
				y = value.Y,
			}
		end

		local function photoUiMode(args): string
			local mode = args.uiMode
			if mode == nil then
				return if args.hideUI == false then "overlay" else "none"
			end
			mode = tostring(mode)
			if mode ~= "none" and mode ~= "overlay" and mode ~= "only" then
				error("uiMode must be none, overlay, or only")
			end
			return mode
		end

		local function cleanupExpiredPhotoSessions()
			local now = os.clock()
			for id, session in pairs(photoSessions) do
				if now - session.createdAt > PHOTO_SESSION_TTL_SECONDS then
					photoSessions[id] = nil
				end
			end
		end

		local function makePhotoSessionRoom()
			cleanupExpiredPhotoSessions()
			local active = {}
			for id, session in pairs(photoSessions) do
				table.insert(active, { id = id, createdAt = session.createdAt })
			end
			-- Evict only when preparing to insert; reads must preserve both valid slots.
			while #active >= PHOTO_MAX_SESSIONS do
				table.sort(active, function(a, b)
					return a.createdAt < b.createdAt
				end)
				local oldest = table.remove(active, 1)
				photoSessions[oldest.id] = nil
			end
		end

		local function schedulePhotoSessionExpiry(sessionId: string)
			-- Defined outside preparePhoto so the delayed closure cannot retain RGBA pixels.
			task.delay(PHOTO_SESSION_TTL_SECONDS, function()
				photoSessions[sessionId] = nil
			end)
		end

		local function preparePhoto(args)
			cleanupExpiredPhotoSessions()
			local Photo = getPhotoModule()
			local background = tostring(args.background or "transparent")
			if background ~= "transparent" and background ~= "scene" then
				error("background must be transparent or scene")
			end
			local uiMode = photoUiMode(args)
			local hideUI = uiMode == "none"
			if uiMode == "only" and background ~= "transparent" then
				error("uiMode only requires a transparent background")
			end
			local outputSize = photoSize(args.outputSize, "outputSize")
			local nativeRect = photoRect(args.nativeRect)
			local cameraCFrame = photoCameraCFrame(args.cameraCFrame)
			local uiTarget = photoUiTarget(args.uiTarget)
			if uiTarget ~= nil and uiMode ~= "only" then
				error("uiTarget requires uiMode only")
			end
			local timeoutSeconds = math.clamp(finiteNumber(args.timeoutSeconds or 120, "timeoutSeconds"), 1, 120)
			local delay = math.clamp(finiteNumber(args.delay or 0, "delay"), 0, 5)
			local options = {
				background = background,
				region = nativeRect,
				size = outputSize,
				delay = delay,
				timeout = timeoutSeconds,
				alphaBleed = args.alphaBleed == true,
				uiMode = uiMode,
				hideUI = hideUI,
				uiTarget = uiTarget,
			}

			local focusPath = tostring(args.focus or "")
			if uiMode == "only" and focusPath ~= "" then
				error("uiMode only captures the current viewport and cannot be combined with focus")
			end
			if cameraCFrame ~= nil and focusPath == "" then
				error("cameraCFrame requires a photo focus target")
			end
			if cameraCFrame ~= nil and args.direction ~= nil then
				error("cameraCFrame cannot be combined with direction")
			end
			local capture
			local isolated = false
			local instanceTightCrop = false
			local fieldOfViewMetadata = nil
			if focusPath ~= "" then
				local target = resolveRemotePath(focusPath)
				if target == nil then
					error("photo focus path not found: " .. focusPath)
				end
				if nativeRect ~= nil then
					error("nativeRect cannot be combined with a photo focus target")
				end
				if outputSize == nil then
					outputSize = Vector2.new(1024, 1024)
					options.size = outputSize
				end
				local padding = finiteNumber(args.padding or 1.25, "padding")
				local fieldOfView = finiteNumber(args.fieldOfView or 32, "fieldOfView")
				if padding < 1 or padding > 4 then
					error("padding must be between 1 and 4")
				end
				if fieldOfView < 1 or fieldOfView > 120 then
					error("fieldOfView must be between 1 and 120")
				end
				fieldOfViewMetadata = fieldOfView
				isolated = args.isolate ~= false
				instanceTightCrop = isolated and background == "transparent" and args.tightCrop ~= false
				capture = Photo.renderInstance(target, {
					view = tostring(args.view or "isometric"),
					direction = photoDirection(args.direction),
					padding = padding,
					tightCrop = instanceTightCrop,
					size = outputSize,
					fieldOfView = fieldOfView,
					cameraCFrame = cameraCFrame,
					background = background,
					delay = delay,
					timeout = timeoutSeconds,
					alphaBleed = args.alphaBleed == true,
					uiMode = uiMode,
					hideUI = hideUI,
					isolate = isolated,
				})
			else
				capture = Photo.captureView(options)
			end
			if type(capture) ~= "table" then
				error("Photo capture returned an invalid result")
			end

			local size = capture.size
			local rawPixels = capture.buffer
			if typeof(size) ~= "Vector2" or typeof(rawPixels) ~= "buffer" then
				error("Photo capture must return a Vector2 size and RGBA buffer")
			end
			local width = math.floor(size.X)
			local height = math.floor(size.Y)
			if
				width < 1
				or height < 1
				or width > PHOTO_MAX_AXIS
				or height > PHOTO_MAX_AXIS
				or width * height > PHOTO_MAX_PIXELS
			then
				error("Photo returned dimensions outside the Ro Sync size limit")
			end

			local expected = width * height * 4
			if buffer.len(rawPixels) ~= expected then
				error("Photo RGBA buffer length does not match its dimensions")
			end
			local pixels = buffer.create(expected)
			buffer.copy(pixels, 0, rawPixels, 0, expected)
			local byteLength = buffer.len(pixels)
			if byteLength ~= width * height * 4 or byteLength > PHOTO_MAX_BYTES then
				error("Photo returned an invalid RGBA byte length")
			end

			local region = photoRegionMetadata(capture.region)
			if region == nil then
				region = photoRegionMetadata(nativeRect)
			end
			local fullSize = photoFullSizeMetadata(capture.fullSize)
			local uiTargetPath = nil
			local uiTargetClass = nil
			if uiTarget ~= nil then
				uiTargetPath = instancePathString(uiTarget)
				uiTargetClass = uiTarget.ClassName
			end
			local regionSource = capture.regionSource
			if regionSource ~= nil and type(regionSource) ~= "string" then
				error("Photo capture regionSource metadata must be a string")
			end
			if regionSource == nil then
				if nativeRect ~= nil then
					regionSource = "explicit"
				elseif uiTarget ~= nil then
					regionSource = "target-alpha"
				elseif focusPath ~= "" and instanceTightCrop then
					regionSource = "subject-alpha"
				elseif focusPath ~= "" then
					regionSource = "framed"
				else
					regionSource = "full"
				end
			end

			photoSessionCounter += 1
			local sessionId = tostring(photoSessionCounter) .. "-" .. HttpService:GenerateGUID(false)
			makePhotoSessionRoom()
			photoSessions[sessionId] = {
				pixels = pixels,
				createdAt = os.clock(),
			}
			schedulePhotoSessionExpiry(sessionId)

			return {
				sessionId = sessionId,
				width = width,
				height = height,
				byteLength = byteLength,
				background = background,
				uiMode = uiMode,
				isolated = isolated,
				tightCrop = instanceTightCrop,
				region = region,
				fullSize = fullSize,
				regionSource = regionSource,
				uiTarget = uiTargetPath,
				uiTargetClass = uiTargetClass,
				cameraCFrame = encodedPhotoCameraCFrame(cameraCFrame),
				fieldOfView = fieldOfViewMetadata,
			}
		end

		remoteHandlers.photo_prepare = function(args)
			if photoPrepareBusy then
				error("another Photo capture is already in progress")
			end
			photoPrepareBusy = true
			local ok, result = pcall(preparePhoto, args)
			photoPrepareBusy = false
			if not ok then
				error(result)
			end
			return result
		end

		remoteHandlers.photo_read = function(args)
			cleanupExpiredPhotoSessions()
			local sessionId = tostring(args.sessionId or "")
			local session = photoSessions[sessionId]
			if session == nil then
				error("photo session not found or expired: " .. sessionId)
			end
			local total = buffer.len(session.pixels)
			local offset = math.floor(tonumber(args.offset) or 0)
			local maxBytes = math.floor(tonumber(args.maxBytes) or PHOTO_MAX_CHUNK_BYTES)
			if offset < 0 or offset >= total then
				error("photo offset is outside the RGBA buffer")
			end
			maxBytes = math.clamp(maxBytes, 1, PHOTO_MAX_CHUNK_BYTES)
			local count = math.min(maxBytes, total - offset)
			local chunk = buffer.fromstring(buffer.readstring(session.pixels, offset, count))
			local nextOffset = offset + count
			return {
				offset = offset,
				nextOffset = nextOffset,
				eof = nextOffset >= total,
				bytesBase64 = remoteState.encodeRemoteBuffer(chunk),
			}
		end

		remoteHandlers.photo_close = function(args)
			local sessionId = tostring(args.sessionId or "")
			photoSessions[sessionId] = nil
			return true
		end
	end

	do
		-- ---------------------------------------------------------------------------
		-- Studio screen capture. Captures stay in Studio as short-lived buffers and
		-- are read in bounded chunks so a 4K image never becomes one enormous JSON
		-- frame. Permission is never requested implicitly: `capture_authorize` is the
		-- only operation allowed to show Studio's privacy prompt.
		-- ---------------------------------------------------------------------------

		local CAPTURE_SESSION_TTL_SECONDS = 120
		local CAPTURE_MAX_CHUNK_BYTES = 512 * 1024
		local CAPTURE_MAX_AXIS = 16 * 1024
		local CAPTURE_MAX_PIXELS = 64 * 1024 * 1024
		local CAPTURE_MAX_BYTES = 128 * 1024 * 1024
		local CAPTURE_MAX_SESSIONS = 4
		local captureSessionCounter = 0
		local captureSessions = {}
		local capturePrepareBusy = false
		local captureProviderUnsupported = false
		local captureProviderError = nil

		local function getStudioCaptureService()
			local ok, service = pcall(function()
				return game:GetService("StudioCaptureService")
			end)
			if not ok or service == nil then
				return nil
			end
			return service
		end

		local function canCaptureScreenshot(service): boolean
			local ok, allowed = pcall(function()
				return service:CanCaptureScreenshot()
			end)
			return ok and allowed == true
		end

		local function isUnsupportedCaptureProviderError(value: any): boolean
			return tostring(value):lower():find("feature not supported yet", 1, true) ~= nil
		end

		local function captureVector(value: any, fieldName: string, allowNegative: boolean?): Vector2?
			if value == nil then
				return nil
			end
			if type(value) ~= "table" then
				error(fieldName .. " must be an object with x and y")
			end
			local x = tonumber(value.x)
			local y = tonumber(value.y)
			if
				x == nil
				or y == nil
				or x ~= x
				or y ~= y
				or x == math.huge
				or y == math.huge
				or x == -math.huge
				or y == -math.huge
				or (not allowNegative and (x < 0 or y < 0))
			then
				error(fieldName .. " must contain valid numeric x and y")
			end
			if not allowNegative and (x > CAPTURE_MAX_AXIS or y > CAPTURE_MAX_AXIS) then
				error(fieldName .. " exceeds the per-axis capture limit")
			end
			return Vector2.new(math.floor(x), math.floor(y))
		end

		local function focusCameraForCapture(path: string, viewName: string, padding: number?): () -> ()
			local target = resolveRemotePath(path)
			if not target then
				error("capture focus path not found: " .. path)
			end

			-- Build the bounds from every rotated part corner. Position +/- Size/2
			-- underestimates rotated parts and is the common reason isometric captures
			-- clip limbs, tools, or diagonal map geometry.
			local corners = {}
			local minimum = Vector3.new(math.huge, math.huge, math.huge)
			local maximum = Vector3.new(-math.huge, -math.huge, -math.huge)
			local function includePart(part: BasePart)
				local half = part.Size * 0.5
				for xSign = -1, 1, 2 do
					for ySign = -1, 1, 2 do
						for zSign = -1, 1, 2 do
							local corner = part.CFrame:PointToWorldSpace(
								Vector3.new(half.X * xSign, half.Y * ySign, half.Z * zSign)
							)
							table.insert(corners, corner)
							minimum = Vector3.new(
								math.min(minimum.X, corner.X),
								math.min(minimum.Y, corner.Y),
								math.min(minimum.Z, corner.Z)
							)
							maximum = Vector3.new(
								math.max(maximum.X, corner.X),
								math.max(maximum.Y, corner.Y),
								math.max(maximum.Z, corner.Z)
							)
						end
					end
				end
			end
			if target:IsA("BasePart") then
				includePart(target)
			end
			for _, descendant in ipairs(target:GetDescendants()) do
				if descendant:IsA("BasePart") then
					includePart(descendant)
				end
			end
			if #corners == 0 then
				error("capture focus target has no BaseParts: " .. path)
			end

			local camera = workspace.CurrentCamera
			if not camera then
				error("Workspace.CurrentCamera is unavailable")
			end
			local oldCFrame = camera.CFrame
			local oldFocus = camera.Focus
			local view = viewName:lower()
			local direction = if view == "front"
				then Vector3.new(0, 0, 1)
				elseif view == "back" then Vector3.new(0, 0, -1)
				elseif view == "left" then Vector3.new(-1, 0, 0)
				elseif view == "right" then Vector3.new(1, 0, 0)
				elseif view == "top" then Vector3.new(0, 1, 0)
				elseif view == "bottom" then Vector3.new(0, -1, 0)
				else Vector3.new(1, 0.75, 1)
			direction = direction.Unit
			local center = (minimum + maximum) * 0.5
			local upHint = if math.abs(direction.Y) > 0.99 then Vector3.new(0, 0, -1) else Vector3.yAxis
			local orientation = CFrame.lookAt(center + direction, center, upHint)
			local right = orientation.RightVector
			local up = orientation.UpVector
			local viewport = camera.ViewportSize
			local aspect = if viewport.Y > 0 then math.max(viewport.X / viewport.Y, 0.01) else 16 / 9
			local verticalTangent = math.tan(math.rad(math.clamp(camera.FieldOfView, 1, 120) * 0.5))
			local horizontalTangent = verticalTangent * aspect
			local margin = math.clamp(tonumber(padding) or 1.25, 1, 4)
			local distance = 1
			local nearestOffset = 0
			for _, corner in ipairs(corners) do
				local relative = corner - center
				local depthOffset = relative:Dot(direction)
				nearestOffset = math.max(nearestOffset, depthOffset)
				distance = math.max(
					distance,
					depthOffset + math.abs(relative:Dot(right)) * margin / horizontalTangent,
					depthOffset + math.abs(relative:Dot(up)) * margin / verticalTangent
				)
			end
			-- Keep the nearest corner safely beyond the camera's near plane even for
			-- very deep subjects whose screen-space extents are otherwise tiny.
			distance = math.max(distance, nearestOffset + 0.5)
			local restored = false
			local function restore()
				if restored then
					return
				end
				restored = true
				pcall(function()
					camera.CFrame = oldCFrame
					camera.Focus = oldFocus
				end)
			end
			local focused, focusError = pcall(function()
				camera.CFrame = CFrame.lookAt(center + direction * distance, center, upHint)
				camera.Focus = CFrame.new(center)
				task.wait(0.1)
			end)
			if not focused then
				restore()
				error("unable to frame capture camera: " .. tostring(focusError))
			end
			return restore
		end

		local function destroyCaptureSession(session)
			if session == nil then
				return
			end
			pcall(function()
				if session.capture then
					session.capture:Destroy()
				end
			end)
		end

		local function cleanupCaptureSessions()
			local now = os.clock()
			local active = {}
			for id, session in pairs(captureSessions) do
				if now - session.createdAt > CAPTURE_SESSION_TTL_SECONDS then
					destroyCaptureSession(session)
					captureSessions[id] = nil
				else
					table.insert(active, { id = id, createdAt = session.createdAt })
				end
			end
			if #active >= CAPTURE_MAX_SESSIONS then
				table.sort(active, function(a, b)
					return a.createdAt < b.createdAt
				end)
				local oldest = active[1]
				destroyCaptureSession(captureSessions[oldest.id])
				captureSessions[oldest.id] = nil
			end
		end

		remoteHandlers.capabilities = function(_args)
			local captureService = getStudioCaptureService()
			local photoChild = script.Parent:FindFirstChild("Photo")
			local photoAvailable = photoChild ~= nil and photoChild:IsA("ModuleScript")
			local playscriptChild = script.Parent:FindFirstChild("Playscript")
			local playscriptAvailable = playscriptChild ~= nil and playscriptChild:IsA("ModuleScript")
			local hostType = hostDataModelTypeName()
			local pluginConnectionOk, pluginConnection = pcall(function()
				local service = game:GetService("PluginConnectionService")
				local anyService: any = service
				if anyService.CanHaveConnectionType ~= nil then
					return service:CanHaveConnectionType(Enum.PluginConnectionTargetType.Test)
				end
				return service ~= nil
			end)
			pluginConnection = pluginConnectionOk and pluginConnection == true
			local studioTestOk, studioTest = pcall(function()
				local service = game:GetService("StudioTestService")
				local anyService: any = service
				return anyService.ExecutePlayModeAsync ~= nil
					and anyService.ExecuteRunModeAsync ~= nil
					and anyService.ExecuteMultiplayerTestAsync ~= nil
			end)
			studioTest = studioTestOk and studioTest == true
			local virtualInputOk, virtualInput = pcall(function()
				local userInput = game:GetService("UserInputService")
				return (userInput :: any).CreateVirtualInput ~= nil
			end)
			virtualInput = virtualInputOk and virtualInput == true
			return {
				pluginVersion = PLUGIN_VERSION_STRING,
				protocolVersion = PLUGIN_PROTOCOL_VERSION,
				hostDataModelType = hostType,
				placeId = tostring(game.PlaceId),
				gameId = tostring(game.GameId),
				features = {
					capture = captureService ~= nil,
					captureAuthorized = captureService ~= nil and canCaptureScreenshot(captureService),
					captureRegion = captureService ~= nil,
					photo = photoAvailable,
					photoTransparent = photoAvailable,
					photoUiOnly = photoAvailable,
					photoCameraCFrame = photoAvailable,
					photoUiTarget = photoAvailable,
					photoInstanceTightCrop = photoAvailable,
					artifactChunks = true,
					instanceClipboard = remoteState.clipboardAvailable == true,
					copyPaste = remoteState.clipboardAvailable == true,
					pluginConnection = pluginConnection,
					studioTest = studioTest,
					virtualInput = virtualInput,
					playtest = studioTest and pluginConnection,
					playtestRun = studioTest and pluginConnection and playscriptAvailable,
					runtimeExec = pluginConnection,
					uiInspect = pluginConnection,
					playtestLogs = pluginConnection,
					virtualInputRouting = pluginConnection and virtualInput,
					workflows = true,
					transactions = true,
					structuredErrors = true,
					stableRefs = true,
				},
				limits = {
					captureChunkBytes = CAPTURE_MAX_CHUNK_BYTES,
					captureAxis = CAPTURE_MAX_AXIS,
					capturePixels = CAPTURE_MAX_PIXELS,
					captureBytes = CAPTURE_MAX_BYTES,
					captureSessions = CAPTURE_MAX_SESSIONS,
					captureSessionTtlSeconds = CAPTURE_SESSION_TTL_SECONDS,
					photoAxis = 4096,
					photoPixels = 16 * 1024 * 1024,
					photoChunkBytes = 512 * 1024,
					clipboardBytes = 128 * 1024 * 1024,
					clipboardRoots = 256,
					playscriptEventBytes = 64 * 1024,
					playscriptResultBytes = 1024 * 1024,
					playscriptResultChunkBytes = 96 * 1024,
					playscriptEventsPerSecond = 20,
					playscriptEventBurst = 40,
					playscriptPollEvents = 64,
					playscriptPollBytes = 512 * 1024,
				},
			}
		end

		remoteHandlers.capture_status = function(_args)
			local service = getStudioCaptureService()
			local photoChild = script.Parent:FindFirstChild("Photo")
			local photoAvailable = photoChild ~= nil and photoChild:IsA("ModuleScript")
			return {
				available = service ~= nil,
				authorized = service ~= nil and canCaptureScreenshot(service),
				photoAvailable = photoAvailable,
				photoUiOnlyAvailable = photoAvailable,
				photoCameraCFrameAvailable = photoAvailable,
				photoUiTargetAvailable = photoAvailable,
				photoInstanceTightCropAvailable = photoAvailable,
				photoAuthorizationRequired = false,
				permissionPromptRequired = service ~= nil
					and not canCaptureScreenshot(service)
					and not captureProviderUnsupported,
				providerUnsupported = captureProviderUnsupported,
				providerError = captureProviderError,
			}
		end

		remoteHandlers.capture_authorize = function(_args)
			local service = getStudioCaptureService()
			if service == nil then
				error("StudioCaptureService is unavailable in this Studio build")
			end
			if captureProviderUnsupported then
				return {
					authorized = false,
					providerUnsupported = true,
					providerError = captureProviderError,
				}
			end
			local ok, authorized = pcall(function()
				return service:RequestScreenshotPermissionAsync()
			end)
			if not ok then
				if isUnsupportedCaptureProviderError(authorized) then
					captureProviderUnsupported = true
					captureProviderError = tostring(authorized)
					return {
						authorized = false,
						providerUnsupported = true,
						providerError = captureProviderError,
					}
				end
				error("screenshot permission request failed: " .. tostring(authorized))
			end
			captureProviderUnsupported = false
			captureProviderError = nil
			return {
				authorized = authorized == true and canCaptureScreenshot(service),
				providerUnsupported = false,
			}
		end

		local function prepareCapture(args)
			cleanupCaptureSessions()
			local service = getStudioCaptureService()
			if service == nil then
				error("StudioCaptureService is unavailable in this Studio build")
			end
			if captureProviderUnsupported then
				error(
					"Studio screenshot provider is unsupported after explicit capture authorization: "
						.. tostring(captureProviderError or "Feature not supported yet")
				)
			end
			if not canCaptureScreenshot(service) then
				error("screenshot permission is not granted; run `rosync capture authorize` first")
			end

			local position = captureVector(args.position, "position", true)
			local captureSize = captureVector(args.captureSize, "captureSize")
			local outputSize = captureVector(args.outputSize, "outputSize")
			if (position == nil) ~= (captureSize == nil) then
				error("position and captureSize must be provided together")
			end
			if captureSize and (captureSize.X < 1 or captureSize.Y < 1) then
				error("captureSize dimensions must be positive")
			end
			if outputSize and (outputSize.X < 1 or outputSize.Y < 1) then
				error("outputSize dimensions must be positive")
			end
			for _, checkedSize in pairs({ captureSize, outputSize }) do
				if checkedSize and checkedSize.X * checkedSize.Y > CAPTURE_MAX_PIXELS then
					error("requested capture exceeds the pixel limit")
				end
			end

			local options = {
				Format = Enum.StudioCaptureScreenshotFormat.PNG,
				UICaptureMode = if tostring(args.ui or "all"):lower() == "none"
					then Enum.UICaptureMode.None
					else Enum.UICaptureMode.All,
				ResampleMode = if tostring(args.resample or "default"):lower() == "pixelated"
					then Enum.ResamplerMode.Pixelated
					else Enum.ResamplerMode.Default,
			}
			if position then
				options.Position = position
				options.CaptureSize = captureSize
			end
			if outputSize then
				options.OutputSize = outputSize
			end
			local restoreCamera = nil
			local focusPath = tostring(args.focus or "")
			if focusPath ~= "" then
				restoreCamera =
					focusCameraForCapture(focusPath, tostring(args.view or "isometric"), tonumber(args.padding))
			end

			local capture = nil
			local okCapture, captureError = pcall(function()
				capture = service:CaptureScreenshot(options)
				if capture == nil then
					error("Studio returned no capture")
				end
				local captureTimeout = math.clamp(tonumber(args.timeoutSeconds) or 30, 0.1, 30)
				local deadline = os.clock() + captureTimeout
				while
					capture.BufferStatus == Enum.StudioCaptureBufferStatus.NotStarted
					or capture.BufferStatus == Enum.StudioCaptureBufferStatus.Pending
				do
					if os.clock() >= deadline then
						error("Studio screenshot timed out")
					end
					task.wait(0.02)
				end
			end)
			-- Restore immediately after Studio has finished reading the viewport. The
			-- protected region above guarantees this runs for capture and polling errors.
			if restoreCamera then
				restoreCamera()
			end
			if not okCapture or capture == nil then
				destroyCaptureSession({ capture = capture })
				error("CaptureScreenshot failed: " .. tostring(captureError))
			end
			if capture.BufferStatus ~= Enum.StudioCaptureBufferStatus.Ready then
				local errors = {}
				pcall(function()
					for _, captureError in ipairs(capture:GetErrors()) do
						table.insert(errors, tostring(captureError))
					end
				end)
				destroyCaptureSession({ capture = capture })
				error("Studio screenshot failed: " .. (#errors > 0 and table.concat(errors, "; ") or "unknown error"))
			end

			local okBuffer, bytes = pcall(function()
				return capture:GetBuffer()
			end)
			if not okBuffer or typeof(bytes) ~= "buffer" then
				destroyCaptureSession({ capture = capture })
				error("GetBuffer failed: " .. tostring(bytes))
			end
			local resolution = capture.Resolution
			if
				resolution.X < 1
				or resolution.Y < 1
				or resolution.X > CAPTURE_MAX_AXIS
				or resolution.Y > CAPTURE_MAX_AXIS
				or resolution.X * resolution.Y > CAPTURE_MAX_PIXELS
			then
				destroyCaptureSession({ capture = capture })
				error("Studio returned invalid screenshot dimensions")
			end
			if buffer.len(bytes) < 1 or buffer.len(bytes) > CAPTURE_MAX_BYTES then
				destroyCaptureSession({ capture = capture })
				error("Studio returned an invalid screenshot byte length")
			end

			captureSessionCounter += 1
			local sessionId = tostring(captureSessionCounter) .. "-" .. HttpService:GenerateGUID(false)
			local session = {
				capture = capture,
				bytes = bytes,
				createdAt = os.clock(),
			}
			captureSessions[sessionId] = session
			task.delay(CAPTURE_SESSION_TTL_SECONDS, function()
				if captureSessions[sessionId] == session then
					destroyCaptureSession(session)
					captureSessions[sessionId] = nil
				end
			end)
			return {
				sessionId = sessionId,
				width = math.floor(resolution.X),
				height = math.floor(resolution.Y),
				byteLength = buffer.len(bytes),
				position = { x = capture.Position.X, y = capture.Position.Y },
				originalSize = { x = capture.OriginalSize.X, y = capture.OriginalSize.Y },
				format = capture.BufferFormat.Name,
				ui = capture.UICaptureMode.Name,
			}
		end

		remoteHandlers.capture_prepare = function(args)
			if capturePrepareBusy then
				error("another screenshot capture is already in progress")
			end
			capturePrepareBusy = true
			local ok, result = pcall(prepareCapture, args)
			capturePrepareBusy = false
			if not ok then
				error(result)
			end
			return result
		end

		remoteHandlers.capture_read = function(args)
			cleanupCaptureSessions()
			local sessionId = tostring(args.sessionId or "")
			local session = captureSessions[sessionId]
			if session == nil then
				error("capture session not found or expired: " .. sessionId)
			end
			local total = buffer.len(session.bytes)
			local offset = math.floor(tonumber(args.offset) or 0)
			local maxBytes = math.floor(tonumber(args.maxBytes) or CAPTURE_MAX_CHUNK_BYTES)
			if offset < 0 or offset >= total then
				error("capture offset is outside the artifact")
			end
			maxBytes = math.clamp(maxBytes, 1, CAPTURE_MAX_CHUNK_BYTES)
			local count = math.min(maxBytes, total - offset)
			local chunk = buffer.fromstring(buffer.readstring(session.bytes, offset, count))
			local nextOffset = offset + count
			return {
				offset = offset,
				nextOffset = nextOffset,
				eof = nextOffset >= total,
				bytesBase64 = remoteState.encodeRemoteBuffer(chunk),
			}
		end

		remoteHandlers.capture_export = function(args)
			cleanupCaptureSessions()
			local sessionId = tostring(args.sessionId or "")
			local session = captureSessions[sessionId]
			if session == nil then
				error("capture session not found or expired: " .. sessionId)
			end
			local lease = args.lease
			if type(lease) ~= "table" then
				error("capture export requires an artifact lease")
			end
			local artifactId = tostring(lease.id or "")
			local token = tostring(lease.token or "")
			if artifactId == "" or token == "" then
				error("artifact lease is missing id or token")
			end

			local total = buffer.len(session.bytes)
			local deadline = os.clock() + math.clamp(tonumber(args.timeoutSeconds) or 30, 0.1, 120)
			local offset = 0
			while offset < total do
				if os.clock() >= deadline then
					error("capture export exceeded its end-to-end deadline")
				end
				local count = math.min(384 * 1024, total - offset)
				local chunk = buffer.fromstring(buffer.readstring(session.bytes, offset, count))
				local ok, response = httpJson("/artifacts/" .. artifactId .. "/chunk", "POST", {
					token = token,
					offset = offset,
					bytesBase64 = remoteState.encodeRemoteBuffer(chunk),
				})
				if not ok or type(response) ~= "table" or response.ok ~= true then
					local detail = if type(response) == "table" then response.error else response
					if type(detail) == "table" then
						detail = detail.message or detail.code
					end
					error("artifact chunk upload failed at offset " .. tostring(offset) .. ": " .. tostring(detail))
				end
				offset += count
			end

			local ok, response = httpJson("/artifacts/" .. artifactId .. "/finalize", "POST", {
				token = token,
				expectedSha256 = args.expectedSha256,
			})
			if not ok or type(response) ~= "table" or response.ok ~= true then
				local detail = if type(response) == "table" then response.error else response
				if type(detail) == "table" then
					detail = detail.message or detail.code
				end
				error("artifact finalization failed: " .. tostring(detail))
			end
			destroyCaptureSession(session)
			captureSessions[sessionId] = nil
			return response.artifact
		end

		remoteHandlers.capture_close = function(args)
			local sessionId = tostring(args.sessionId or "")
			local session = captureSessions[sessionId]
			if session then
				destroyCaptureSession(session)
				captureSessions[sessionId] = nil
			end
			return true
		end
	end

	do
		-- ---------------------------------------------------------------------------
		-- Tier 1: construction / destruction / reparent / attributes / tags / method
		-- calls / selection. These all POST a /writelog entry on success (see
		-- REMOTE_WRITE_OPS below).
		-- ---------------------------------------------------------------------------

		local CollectionService = game:GetService("CollectionService")
		local Selection = game:GetService("Selection")

		remoteHandlers.new = function(args)
			local parentPath = tostring(args.parent or args.path or "")
			local parent = resolveRemotePath(parentPath)
			if not parent then
				error("parent not found: " .. parentPath)
			end
			local className = tostring(args.class or "")
			if className == "" then
				error("missing class")
			end
			local createOk, inst = pcall(Instance.new, className)
			if not createOk or not inst then
				error("Instance.new failed for class " .. className .. ": " .. tostring(inst))
			end
			if args.name ~= nil then
				inst.Name = tostring(args.name)
			end
			local props = args.initial_props or args.props
			local pendingSource = nil
			if type(props) == "table" then
				for k, v in pairs(props) do
					local key = tostring(k)
					local decoded = decodeRemoteValue(v)
					-- Skip Parent here — we set it last so every other property is in
					-- place before AncestryChanged fires.
					if key == "Source" and inst:IsA("LuaSourceContainer") then
						pendingSource = tostring(decoded or "")
					elseif key ~= "Parent" then
						inst[key] = decoded
					end
				end
			end
			inst.Parent = parent
			if pendingSource ~= nil and not writeScriptSource(inst, pendingSource, false) then
				inst:Destroy()
				error("source write was not applied for new " .. className)
			end
			return {
				path = instancePathString(inst),
				class = inst.ClassName,
				name = inst.Name,
				etag = remoteState.instanceEtag(inst),
			}
		end

		remoteHandlers.rm = function(args)
			local inst = resolveRemotePath(tostring(args.path or ""))
			if not inst then
				error("path not found: " .. tostring(args.path))
			end
			if inst == game then
				error("cannot destroy the DataModel root")
			end
			local path = instancePathString(inst)
			inst:Destroy()
			return { path = path, destroyed = true }
		end

		remoteHandlers.mv = function(args)
			local fromPath = tostring(args.from or "")
			local toPath = tostring(args.to or "")
			local inst = resolveRemotePath(fromPath)
			if not inst then
				error("from not found: " .. fromPath)
			end
			if inst == game then
				error("cannot reparent the DataModel root")
			end
			local newParent = resolveRemotePath(toPath)
			if not newParent then
				error("to not found: " .. toPath)
			end
			-- Top-level service boundary = segs[1]. Cross-service reparenting requires
			-- args.force because dropping an instance into the wrong service
			-- (e.g. Workspace -> ServerStorage) usually means losing replication.
			local oldSegs = splitRemotePath(instancePathString(inst))
			local newParentSegs = splitRemotePath(instancePathString(newParent))
			local oldService = oldSegs[1] or ""
			local newService = newParentSegs[1] or ""
			if oldService ~= newService and not args.force then
				error(
					string.format(
						"refusing to reparent across service boundary %q -> %q (pass force=true to override)",
						oldService,
						newService
					)
				)
			end
			inst.Parent = newParent
			return {
				path = instancePathString(inst),
				parent = instancePathString(newParent),
				etag = remoteState.instanceEtag(inst),
			}
		end

		remoteHandlers.set_attr = function(args)
			local inst = resolveRemotePath(tostring(args.path or ""))
			if not inst then
				error("path not found: " .. tostring(args.path))
			end
			local name = tostring(args.name or "")
			if name == "" then
				error("missing attribute name")
			end
			local decoded = decodeRemoteValue(args.value)
			inst:SetAttribute(name, decoded)
			return { path = instancePathString(inst), name = name }
		end

		remoteHandlers.rm_attr = function(args)
			local inst = resolveRemotePath(tostring(args.path or ""))
			if not inst then
				error("path not found: " .. tostring(args.path))
			end
			local name = tostring(args.name or "")
			if name == "" then
				error("missing attribute name")
			end
			inst:SetAttribute(name, nil)
			return { path = instancePathString(inst), name = name, cleared = true }
		end

		remoteHandlers.attr_ls = function(args)
			local inst = resolveRemotePath(tostring(args.path or ""))
			if not inst then
				error("path not found: " .. tostring(args.path))
			end
			local out = {}
			local aok, raw = pcall(function()
				return inst:GetAttributes()
			end)
			if aok and typeof(raw) == "table" then
				for k, v in pairs(raw) do
					local eok, encoded = pcall(encodeRemoteValue, v)
					if eok then
						out[tostring(k)] = encoded
					end
				end
			end
			return out
		end

		remoteHandlers.add_tag = function(args)
			local inst = resolveRemotePath(tostring(args.path or ""))
			if not inst then
				error("path not found: " .. tostring(args.path))
			end
			local tag = tostring(args.tag or "")
			if tag == "" then
				error("missing tag")
			end
			CollectionService:AddTag(inst, tag)
			return { path = instancePathString(inst), tag = tag, added = true }
		end

		remoteHandlers.rm_tag = function(args)
			local inst = resolveRemotePath(tostring(args.path or ""))
			if not inst then
				error("path not found: " .. tostring(args.path))
			end
			local tag = tostring(args.tag or "")
			if tag == "" then
				error("missing tag")
			end
			CollectionService:RemoveTag(inst, tag)
			return { path = instancePathString(inst), tag = tag, removed = true }
		end

		remoteHandlers.tag_ls = function(args)
			local inst = resolveRemotePath(tostring(args.path or ""))
			if not inst then
				error("path not found: " .. tostring(args.path))
			end
			local tags = CollectionService:GetTags(inst)
			table.sort(tags)
			return tags
		end

		remoteHandlers.call = function(args)
			local inst = resolveRemotePath(tostring(args.path or ""))
			if not inst then
				error("path not found: " .. tostring(args.path))
			end
			local method = tostring(args.method or "")
			if method == "" then
				error("missing method")
			end
			local rawArgs = args.args
			local decoded = {}
			local n = 0
			if type(rawArgs) == "table" then
				n = #rawArgs
				for i = 1, n do
					decoded[i] = decodeRemoteValue(rawArgs[i])
				end
			end
			local fn = inst[method]
			if type(fn) ~= "function" then
				error("method not callable: " .. method)
			end
			local callResults = table.pack(fn(inst, table.unpack(decoded, 1, n)))
			if callResults.n == 0 then
				return nil
			end
			if callResults.n == 1 then
				return encodeRemoteValue(callResults[1])
			end
			local callOut = {}
			local encodeState = { nodes = 0, stringBytes = 0, seen = {} }
			for i = 1, callResults.n do
				callOut[i] = encodeRemoteValue(callResults[i], 0, encodeState)
			end
			return callOut
		end

		remoteHandlers.select_get = function(_args)
			local out = {}
			local selOk, sel = pcall(function()
				return Selection:Get()
			end)
			if selOk and typeof(sel) == "table" then
				for _, inst in ipairs(sel) do
					table.insert(out, instancePathString(inst))
				end
			end
			return out
		end

		remoteHandlers.select_set = function(args)
			local paths = args.paths
			if type(paths) ~= "table" then
				error("paths must be an array")
			end
			local list = {}
			for i, p in ipairs(paths) do
				local inst = resolveRemotePath(tostring(p))
				if not inst then
					error("path not found (index " .. i .. "): " .. tostring(p))
				end
				table.insert(list, inst)
			end
			Selection:Set(list)
			return { count = #list }
		end

		remoteHandlers.logs = function(args)
			local sinceSeconds = tonumber(args.since_seconds)
			local sinceSeq = tonumber(args.since_seq)
			local levelMin = args.level_min and tostring(args.level_min) or nil
			local limit = tonumber(args.limit)
			local entries = readLogRing(sinceSeconds, sinceSeq, levelMin, limit)
			return { entries = entries, now = os.clock(), wall = os.time() }
		end

		remoteHandlers.save = function(_args)
			-- game:Save() can take several seconds; yielding here would blow the WS
			-- request timeout. Fire-and-forget; surface any error to the plugin log.
			task.spawn(function()
				local ok, err = pcall(function()
					(game :: any):Save()
				end)
				if not ok then
					warn("[Ro Sync] game:Save() failed: " .. tostring(err))
				end
			end)
			return { ok = true, started = true }
		end

		remoteHandlers.undo = function(_args)
			ChangeHistoryService:Undo()
			return { ok = true }
		end

		remoteHandlers.redo = function(_args)
			ChangeHistoryService:Redo()
			return { ok = true }
		end

		remoteHandlers.waypoint = function(args)
			local name = tostring(args.name or "")
			if name == "" then
				error("missing name")
			end
			ChangeHistoryService:SetWaypoint(name)
			return { ok = true, name = name }
		end

		remoteHandlers.version = function(_args)
			local studioVersion
			local ok, v = pcall(function()
				return game:GetService("RunService"):GetRobloxVersion()
			end)
			if ok then
				studioVersion = tostring(v)
			end
			return {
				plugin_version = PLUGIN_VERSION_STRING,
				protocol = PLUGIN_PROTOCOL_VERSION,
				studio_version = studioVersion,
			}
		end

		remoteHandlers.ping = function(_args)
			return { pong = os.clock() }
		end

		-- ----------------------------------------------------------------------------
		-- Class / enum / attribute introspection
		--
		-- `class_info` tries a StudioService reflection path first (APIs vary by
		-- Studio build and may not be available). It falls back to a baked table
		-- covering the 20 most-inspected classes — not exhaustive, but enough for
		-- agents to discover the shape of a class before they `get --prop` specific
		-- values.
		-- ----------------------------------------------------------------------------
		local BUILTIN_CLASS_INFO = {
			BasePart = {
				properties = {
					{ name = "Name", category = "Data", type = "string" },
					{ name = "Parent", category = "Data", type = "Instance" },
					{ name = "ClassName", category = "Data", type = "string" },
					{ name = "Anchored", category = "Behavior", type = "bool" },
					{ name = "CanCollide", category = "Behavior", type = "bool" },
					{ name = "CanTouch", category = "Behavior", type = "bool" },
					{ name = "CanQuery", category = "Behavior", type = "bool" },
					{ name = "CastShadow", category = "Appearance", type = "bool" },
					{ name = "Massless", category = "Behavior", type = "bool" },
					{ name = "Position", category = "Data", type = "Vector3" },
					{ name = "Size", category = "Data", type = "Vector3" },
					{ name = "Orientation", category = "Data", type = "Vector3" },
					{ name = "CFrame", category = "Data", type = "CFrame" },
					{ name = "Transparency", category = "Appearance", type = "float" },
					{ name = "Reflectance", category = "Appearance", type = "float" },
					{ name = "Material", category = "Appearance", type = "Material" },
					{ name = "Color", category = "Appearance", type = "Color3" },
					{ name = "BrickColor", category = "Appearance", type = "BrickColor" },
					{ name = "CollisionGroup", category = "Behavior", type = "string" },
					{ name = "Shape", category = "Shape", type = "Shape" },
				},
				methods = {
					"GetMass",
					"GetNetworkOwner",
					"GetNetworkOwnershipAuto",
					"GetRootPart",
					"GetTouchingParts",
					"GetConnectedParts",
					"GetJoints",
					"GetPivot",
					"IsA",
					"IsDescendantOf",
					"IsAncestorOf",
					"FindFirstChild",
					"FindFirstAncestor",
					"FindFirstAncestorOfClass",
					"FindFirstAncestorWhichIsA",
					"GetChildren",
					"GetDescendants",
					"GetFullName",
					"Destroy",
					"Clone",
				},
			},
			Part = {
				properties = {
					{ name = "Name", category = "Data", type = "string" },
					{ name = "Parent", category = "Data", type = "Instance" },
					{ name = "Anchored", category = "Behavior", type = "bool" },
					{ name = "CanCollide", category = "Behavior", type = "bool" },
					{ name = "Shape", category = "Shape", type = "Shape" },
					{ name = "Position", category = "Data", type = "Vector3" },
					{ name = "Size", category = "Data", type = "Vector3" },
					{ name = "CFrame", category = "Data", type = "CFrame" },
					{ name = "Transparency", category = "Appearance", type = "float" },
					{ name = "Material", category = "Appearance", type = "Material" },
					{ name = "Color", category = "Appearance", type = "Color3" },
				},
				methods = { "Destroy", "Clone", "IsA", "FindFirstChild", "GetChildren", "GetFullName" },
			},
			MeshPart = {
				properties = {
					{ name = "Name", category = "Data", type = "string" },
					{ name = "Parent", category = "Data", type = "Instance" },
					{ name = "MeshId", category = "Data", type = "Content" },
					{ name = "TextureContent", category = "Appearance", type = "Content" },
					{ name = "TextureID", category = "Appearance", type = "Content" },
					{ name = "Size", category = "Data", type = "Vector3" },
					{ name = "CFrame", category = "Data", type = "CFrame" },
					{ name = "Material", category = "Appearance", type = "Material" },
					{ name = "Anchored", category = "Behavior", type = "bool" },
					{ name = "CanCollide", category = "Behavior", type = "bool" },
				},
				methods = { "Destroy", "Clone", "IsA", "FindFirstChild", "GetChildren", "GetFullName" },
			},
			Model = {
				properties = {
					{ name = "Name", category = "Data", type = "string" },
					{ name = "Parent", category = "Data", type = "Instance" },
					{ name = "PrimaryPart", category = "Behavior", type = "Instance" },
					{ name = "WorldPivot", category = "Data", type = "CFrame" },
					{ name = "ModelStreamingMode", category = "Behavior", type = "ModelStreamingMode" },
					{ name = "LevelOfDetail", category = "Appearance", type = "ModelLevelOfDetail" },
				},
				methods = {
					"GetBoundingBox",
					"GetExtentsSize",
					"GetPivot",
					"PivotTo",
					"MoveTo",
					"TranslateBy",
					"ScaleTo",
					"GetScale",
					"Destroy",
					"Clone",
					"IsA",
					"GetChildren",
					"GetDescendants",
					"GetFullName",
				},
			},
			Folder = {
				properties = {
					{ name = "Name", category = "Data", type = "string" },
					{ name = "Parent", category = "Data", type = "Instance" },
				},
				methods = {
					"Destroy",
					"Clone",
					"IsA",
					"FindFirstChild",
					"GetChildren",
					"GetDescendants",
					"GetFullName",
				},
			},
			Script = {
				properties = {
					{ name = "Name", category = "Data", type = "string" },
					{ name = "Parent", category = "Data", type = "Instance" },
					{ name = "Source", category = "Data", type = "string" },
					{ name = "Disabled", category = "Behavior", type = "bool" },
					{ name = "RunContext", category = "Behavior", type = "RunContext" },
				},
				methods = { "Destroy", "Clone", "IsA", "GetFullName" },
			},
			LocalScript = {
				properties = {
					{ name = "Name", category = "Data", type = "string" },
					{ name = "Parent", category = "Data", type = "Instance" },
					{ name = "Source", category = "Data", type = "string" },
					{ name = "Disabled", category = "Behavior", type = "bool" },
				},
				methods = { "Destroy", "Clone", "IsA", "GetFullName" },
			},
			ModuleScript = {
				properties = {
					{ name = "Name", category = "Data", type = "string" },
					{ name = "Parent", category = "Data", type = "Instance" },
					{ name = "Source", category = "Data", type = "string" },
				},
				methods = { "Destroy", "Clone", "IsA", "GetFullName" },
			},
			RemoteEvent = {
				properties = {
					{ name = "Name", category = "Data", type = "string" },
					{ name = "Parent", category = "Data", type = "Instance" },
				},
				methods = { "FireAllClients", "FireClient", "FireServer", "Destroy", "Clone" },
			},
			RemoteFunction = {
				properties = {
					{ name = "Name", category = "Data", type = "string" },
					{ name = "Parent", category = "Data", type = "Instance" },
				},
				methods = { "InvokeClient", "InvokeServer", "Destroy", "Clone" },
			},
			Camera = {
				properties = {
					{ name = "Name", category = "Data", type = "string" },
					{ name = "CFrame", category = "Data", type = "CFrame" },
					{ name = "Focus", category = "Data", type = "CFrame" },
					{ name = "FieldOfView", category = "Appearance", type = "float" },
					{ name = "CameraType", category = "Behavior", type = "CameraType" },
					{ name = "CameraSubject", category = "Behavior", type = "Instance" },
					{ name = "HeadScale", category = "Appearance", type = "float" },
				},
				methods = { "GetRenderCFrame", "ScreenPointToRay", "WorldToScreenPoint", "WorldToViewportPoint" },
			},
			Lighting = {
				properties = {
					{ name = "Ambient", category = "Appearance", type = "Color3" },
					{ name = "OutdoorAmbient", category = "Appearance", type = "Color3" },
					{ name = "Brightness", category = "Appearance", type = "float" },
					{ name = "ClockTime", category = "Data", type = "float" },
					{ name = "TimeOfDay", category = "Data", type = "string" },
					{ name = "GeographicLatitude", category = "Data", type = "float" },
					{ name = "GlobalShadows", category = "Appearance", type = "bool" },
					{ name = "FogStart", category = "Appearance", type = "float" },
					{ name = "FogEnd", category = "Appearance", type = "float" },
					{ name = "FogColor", category = "Appearance", type = "Color3" },
					{ name = "Technology", category = "Appearance", type = "Technology" },
				},
				methods = { "GetSunDirection", "GetMoonDirection", "SetMinutesAfterMidnight" },
			},
			Workspace = {
				properties = {
					{ name = "Gravity", category = "Behavior", type = "float" },
					{ name = "FallenPartsDestroyHeight", category = "Behavior", type = "float" },
					{ name = "StreamingEnabled", category = "Behavior", type = "bool" },
					{ name = "StreamingTargetRadius", category = "Behavior", type = "int" },
					{ name = "CurrentCamera", category = "Data", type = "Instance" },
				},
				methods = {
					"Raycast",
					"FindPartOnRay",
					"FindPartOnRayWithIgnoreList",
					"GetPartBoundsInBox",
					"GetPartsInPart",
					"GetDescendants",
					"GetChildren",
				},
			},
			Players = {
				properties = {
					{ name = "CharacterAutoLoads", category = "Behavior", type = "bool" },
					{ name = "RespawnTime", category = "Behavior", type = "float" },
					{ name = "MaxPlayers", category = "Data", type = "int" },
					{ name = "PreferredPlayers", category = "Data", type = "int" },
				},
				methods = { "GetPlayers", "GetPlayerFromCharacter", "GetPlayerByUserId", "Chat" },
			},
			ReplicatedStorage = {
				properties = {
					{ name = "Name", category = "Data", type = "string" },
				},
				methods = { "GetChildren", "GetDescendants", "FindFirstChild", "WaitForChild" },
			},
			Frame = {
				properties = {
					{ name = "Name", category = "Data", type = "string" },
					{ name = "Parent", category = "Data", type = "Instance" },
					{ name = "Position", category = "Data", type = "UDim2" },
					{ name = "Size", category = "Data", type = "UDim2" },
					{ name = "AnchorPoint", category = "Data", type = "Vector2" },
					{ name = "BackgroundColor3", category = "Appearance", type = "Color3" },
					{ name = "BackgroundTransparency", category = "Appearance", type = "float" },
					{ name = "BorderSizePixel", category = "Appearance", type = "int" },
					{ name = "BorderColor3", category = "Appearance", type = "Color3" },
					{ name = "ZIndex", category = "Behavior", type = "int" },
					{ name = "Visible", category = "Behavior", type = "bool" },
					{ name = "ClipsDescendants", category = "Behavior", type = "bool" },
					{ name = "LayoutOrder", category = "Behavior", type = "int" },
				},
				methods = { "Destroy", "Clone", "IsA", "FindFirstChild", "GetChildren", "GetFullName" },
			},
			TextLabel = {
				properties = {
					{ name = "Name", category = "Data", type = "string" },
					{ name = "Parent", category = "Data", type = "Instance" },
					{ name = "Text", category = "Text", type = "string" },
					{ name = "TextColor3", category = "Text", type = "Color3" },
					{ name = "TextSize", category = "Text", type = "float" },
					{ name = "TextScaled", category = "Text", type = "bool" },
					{ name = "Font", category = "Text", type = "Font" },
					{ name = "FontFace", category = "Text", type = "FontFace" },
					{ name = "TextXAlignment", category = "Text", type = "TextXAlignment" },
					{ name = "TextYAlignment", category = "Text", type = "TextYAlignment" },
					{ name = "RichText", category = "Text", type = "bool" },
					{ name = "TextWrapped", category = "Text", type = "bool" },
					{ name = "Position", category = "Data", type = "UDim2" },
					{ name = "Size", category = "Data", type = "UDim2" },
					{ name = "BackgroundColor3", category = "Appearance", type = "Color3" },
					{ name = "BackgroundTransparency", category = "Appearance", type = "float" },
				},
				methods = { "Destroy", "Clone", "IsA", "GetFullName" },
			},
			TextButton = {
				properties = {
					{ name = "Name", category = "Data", type = "string" },
					{ name = "Parent", category = "Data", type = "Instance" },
					{ name = "Text", category = "Text", type = "string" },
					{ name = "TextColor3", category = "Text", type = "Color3" },
					{ name = "TextSize", category = "Text", type = "float" },
					{ name = "Font", category = "Text", type = "Font" },
					{ name = "AutoButtonColor", category = "Behavior", type = "bool" },
					{ name = "Active", category = "Behavior", type = "bool" },
					{ name = "Position", category = "Data", type = "UDim2" },
					{ name = "Size", category = "Data", type = "UDim2" },
					{ name = "BackgroundColor3", category = "Appearance", type = "Color3" },
				},
				methods = { "Destroy", "Clone", "IsA", "GetFullName" },
			},
			ImageLabel = {
				properties = {
					{ name = "Name", category = "Data", type = "string" },
					{ name = "Parent", category = "Data", type = "Instance" },
					{ name = "Image", category = "Image", type = "Content" },
					{ name = "ImageColor3", category = "Image", type = "Color3" },
					{ name = "ImageTransparency", category = "Image", type = "float" },
					{ name = "ScaleType", category = "Image", type = "ScaleType" },
					{ name = "SliceCenter", category = "Image", type = "Rect" },
					{ name = "SliceScale", category = "Image", type = "float" },
					{ name = "Position", category = "Data", type = "UDim2" },
					{ name = "Size", category = "Data", type = "UDim2" },
					{ name = "BackgroundTransparency", category = "Appearance", type = "float" },
				},
				methods = { "Destroy", "Clone", "IsA", "GetFullName" },
			},
			ScrollingFrame = {
				properties = {
					{ name = "Name", category = "Data", type = "string" },
					{ name = "Parent", category = "Data", type = "Instance" },
					{ name = "CanvasSize", category = "Data", type = "UDim2" },
					{ name = "CanvasPosition", category = "Data", type = "Vector2" },
					{ name = "AutomaticCanvasSize", category = "Data", type = "AutomaticSize" },
					{ name = "ScrollingDirection", category = "Data", type = "ScrollingDirection" },
					{ name = "ScrollBarThickness", category = "Appearance", type = "int" },
					{ name = "ScrollingEnabled", category = "Behavior", type = "bool" },
					{ name = "Position", category = "Data", type = "UDim2" },
					{ name = "Size", category = "Data", type = "UDim2" },
					{ name = "BackgroundColor3", category = "Appearance", type = "Color3" },
				},
				methods = { "Destroy", "Clone", "IsA", "GetFullName" },
			},
		}

		remoteHandlers.class_info = function(args)
			local className = tostring(args.class_name or args.className or "")
			if className == "" then
				error("missing class_name")
			end
			-- Try StudioService reflection first; fall back to the baked table.
			-- StudioService:GetClassProperties isn't exposed on every Studio build,
			-- so any failure is treated as "not available".
			local ok, ss = pcall(function()
				return game:GetService("StudioService")
			end)
			if ok and ss then
				local getOk, props = pcall(function()
					if typeof(ss.GetClassProperties) == "function" then
						return ss:GetClassProperties(className)
					end
					return nil
				end)
				if getOk and type(props) == "table" and #props > 0 then
					local out = {}
					for _, p in ipairs(props) do
						if type(p) == "table" then
							local vt = p.ValueType
							local typeStr
							if type(vt) == "table" then
								typeStr = tostring(vt.Name or vt)
							elseif vt ~= nil then
								typeStr = tostring(vt)
							else
								typeStr = tostring(p.Type or p.type or "")
							end
							table.insert(out, {
								name = tostring(p.Name or p.name or ""),
								category = tostring(p.Category or p.category or ""),
								type = typeStr,
							})
						end
					end
					if #out > 0 then
						return { properties = out, methods = {} }
					end
				end
			end
			local info = BUILTIN_CLASS_INFO[className]
			if not info then
				error("no class info available for: " .. className)
			end
			return { properties = info.properties, methods = info.methods }
		end

		remoteHandlers.enum_list = function(args)
			local enumName = tostring(args.enum_name or args.name or "")
			if enumName == "" then
				error("missing enum_name")
			end
			local ok, enum = pcall(function()
				return Enum[enumName]
			end)
			if not ok or enum == nil then
				error("unknown enum: " .. enumName)
			end
			local itemsOk, rawItems = pcall(function()
				return enum:GetEnumItems()
			end)
			if not itemsOk or type(rawItems) ~= "table" then
				error("failed to read items for Enum." .. enumName)
			end
			local items = {}
			for _, item in ipairs(rawItems) do
				table.insert(items, { name = tostring(item.Name), value = item.Value })
			end
			return items
		end

		remoteHandlers.enums = function(_args)
			local ok, enums = pcall(function()
				return Enum:GetEnums()
			end)
			if not ok or type(enums) ~= "table" then
				error("Enum:GetEnums() not available")
			end
			local out = {}
			for _, e in ipairs(enums) do
				local s = tostring(e)
				if s:sub(1, 5) == "Enum." then
					s = s:sub(6)
				end
				table.insert(out, s)
			end
			table.sort(out)
			return out
		end

		remoteHandlers.find_by_attr = function(args)
			local attrName = tostring(args.name or "")
			if attrName == "" then
				error("missing name")
			end
			local rootPath = args.under or args.root or ""
			local root = resolveRemotePath(tostring(rootPath))
			if not root then
				error("root not found: " .. tostring(rootPath))
			end
			local limit = tonumber(args.limit) or FIND_LIMIT_DEFAULT
			local hasValue = args.value ~= nil
			local target = nil
			if hasValue then
				local dok, decoded = pcall(decodeRemoteValue, args.value)
				if not dok then
					error("decode value: " .. tostring(decoded))
				end
				target = decoded
			end
			local function equal(a, b)
				if a == b then
					return true
				end
				local ta, tb = typeof(a), typeof(b)
				if ta ~= tb then
					return false
				end
				-- Userdata types whose == isn't structural — fall back to tostring.
				if
					ta == "Vector3"
					or ta == "Vector2"
					or ta == "Color3"
					or ta == "UDim"
					or ta == "UDim2"
					or ta == "CFrame"
					or ta == "BrickColor"
					or ta == "EnumItem"
					or ta == "NumberRange"
				then
					return tostring(a) == tostring(b)
				end
				return false
			end
			local out = {}
			local function walk(inst)
				for _, c in ipairs(inst:GetChildren()) do
					if #out >= limit then
						return
					end
					local ok, val = pcall(function()
						return c:GetAttribute(attrName)
					end)
					if ok and val ~= nil then
						if not hasValue or equal(val, target) then
							table.insert(out, instancePathString(c))
						end
					end
					walk(c)
				end
			end
			walk(root)
			return out
		end
	end

	do
		local REMOTE_WRITE_OPS = {
			set = true,
			eval = true,
			transmit_prepare = true,
			capture_authorize = true,
			capture_prepare = true,
			capture_export = true,
			photo_prepare = true,
			playtest_start = true,
			playtest_run_start = true,
			playtest_run_cancel = true,
			playtest_stop = true,
			playtest_request = true,
			playtest_capture = true,
			transaction_begin = true,
			transaction_finish = true,
			undo = true,
			redo = true,
			save = true,
			waypoint = true,
			new = true,
			rm = true,
			mv = true,
			set_attr = true,
			rm_attr = true,
			add_tag = true,
			rm_tag = true,
			call = true,
			select_set = true,
			clipboard_copy = true,
			clipboard_paste = true,
		}
		-- Runtime requests are still audited as writes, but they mutate temporary,
		-- generation-scoped playtest DataModels and have their own correlated
		-- responses. Serializing them behind the edit-mode transaction lane causes
		-- unrelated server/client execs to exhaust the queue while a peer yields.
		local REMOTE_CONCURRENT_WRITE_OPS = {
			playtest_request = true,
			playtest_run_cancel = true,
		}

		local function redactRemoteAuditValue(value: any, depth: number?, seen: any?): any
			local currentDepth = depth or 0
			local visited = seen or {}
			if currentDepth > 8 then
				return "<max-depth>"
			end
			if type(value) == "string" then
				if #value > 16 * 1024 then
					return string.sub(value, 1, 16 * 1024) .. "<truncated>"
				end
				return value
			end
			if type(value) ~= "table" then
				return value
			end
			if visited[value] then
				return "<cycle>"
			end
			visited[value] = true
			local out = {}
			local count = 0
			for key, child in pairs(value) do
				count += 1
				if count > 500 then
					out.__rosyncTruncated = true
					break
				end
				local normalizedKey = tostring(key):lower():gsub("[^%w]", "")
				if
					normalizedKey == "token"
					or normalizedKey:find("apikey", 1, true)
					or normalizedKey:find("authorization", 1, true)
					or normalizedKey:find("credential", 1, true)
					or normalizedKey:find("password", 1, true)
					or normalizedKey:find("secret", 1, true)
				then
					out[key] = "[REDACTED]"
				else
					out[key] = redactRemoteAuditValue(child, currentDepth + 1, visited)
				end
			end
			visited[value] = nil
			return out
		end

		local function postRemoteWriteLogSync(entry)
			local redacted = redactRemoteAuditValue(entry)
			local ok, response = httpJson("/writelog", "POST", redacted)
			if ok and (type(response) ~= "table" or response.ok ~= true) then
				ok = false
				response = if type(response) == "table"
					then tostring(response.error or "writelog rejected the audit entry")
					else "invalid writelog response"
			end
			if not ok then
				warn("[Ro Sync] writelog POST failed: " .. tostring(response))
			end
			return ok, response
		end

		local function postRemoteWriteLog(entry)
			task.spawn(function()
				postRemoteWriteLogSync(entry)
			end)
		end
		remoteState.postRemoteWriteLog = postRemoteWriteLog
		remoteState.postRemoteWriteLogSync = postRemoteWriteLogSync
		remoteState.preAuditedWrites = setmetatable({}, { __mode = "k" })

		local function remoteAuditArgs(op, args)
			local transform = remoteState.auditArgsForOp
			if transform then
				local ok, transformed = pcall(transform, op, args)
				if ok and type(transformed) == "table" then
					return transformed
				end
			end
			return args
		end

		-- Every remote write enters one cooperative lane. Roblox event callbacks can
		-- overlap when a handler yields, so a plain boolean mutex is insufficient:
		-- transaction_finish could otherwise cancel/commit while an earlier
		-- UpdateSourceAsync, screenshot, playtest request, or eval is still running.
		local REMOTE_WRITE_QUEUE_MAX = 64
		local REMOTE_WRITE_QUEUE_WAIT_SECONDS = 10
		local REMOTE_WRITE_HANDLER_RESPONSE_SECONDS = 25
		local REMOTE_TRANSACTION_HANDLER_RESPONSE_SECONDS = 5
		local remoteWriteQueue = {}
		local remoteWriteActiveItem = nil
		local staleWritesDropped = 0
		local remoteWriteWorkerRunning = false
		local remoteWriteActive = false
		local remoteWriteCleanupRequested = false
		local runRemoteWriteWorker

		local function remoteWriteResponseBound(item): number
			if item.op == "transaction_begin" or item.op == "transaction_finish" then
				return REMOTE_TRANSACTION_HANDLER_RESPONSE_SECONDS
			elseif item.op == "capture_prepare" then
				return math.clamp(tonumber(item.args.timeoutSeconds) or 30, 0.1, 30) + 5
			elseif item.op == "capture_export" then
				return math.clamp(tonumber(item.args.timeoutSeconds) or 30, 0.1, 120) + 5
			elseif item.op == "photo_prepare" then
				return math.clamp(tonumber(item.args.timeoutSeconds) or 120, 1, 120) + 5
			elseif item.op == "playtest_request" then
				return math.clamp(tonumber(item.args.timeout) or 15, 0.1, 120) + 5
			elseif item.op == "playtest_capture" then
				return math.clamp(tonumber(item.args.timeout) or 30, 1, 120) + 5
			elseif item.op == "capture_authorize" then
				-- The privacy prompt is user-driven, but it must not retain the write lane
				-- forever if Studio never completes the authorization request.
				return 120
			elseif item.op == "clipboard_copy" or item.op == "clipboard_paste" then
				return math.clamp(tonumber(item.args.timeoutSeconds) or 120, 1, 300) + 5
			end
			return REMOTE_WRITE_HANDLER_RESPONSE_SECONDS
		end

		local function classifyRemoteHandlerError(op: string, result: any)
			local message = tostring(result)
			local lower = message:lower()
			local code = "PLUGIN_ERROR"
			local retryable = false
			if lower:find("not found", 1, true) then
				code = "NOT_FOUND"
			elseif lower:find("permission", 1, true) or lower:find("authorize", 1, true) then
				code = "PERMISSION_REQUIRED"
			elseif lower:find("timed out", 1, true) or lower:find("timeout", 1, true) then
				code = "TIMEOUT"
				retryable = true
			elseif
				lower:find("missing", 1, true)
				or lower:find("invalid", 1, true)
				or lower:find("must be", 1, true)
			then
				code = "INVALID_ARGUMENT"
			elseif lower:find("already active", 1, true) or lower:find("in progress", 1, true) then
				code = "CONFLICT"
			elseif lower:find("precondition failed", 1, true) then
				code = "PRECONDITION_FAILED"
			end
			return {
				code = code,
				message = message,
				retryable = retryable,
				details = { op = op },
			}
		end

		local function remoteRequestMeta(item)
			return {
				op = item.op,
				durationMs = math.floor((os.clock() - item.receivedAt) * 1000),
				queueMs = if item.executionStartedAt
					then math.floor((item.executionStartedAt - item.receivedAt) * 1000)
					else math.floor((os.clock() - item.receivedAt) * 1000),
				protocol = PLUGIN_PROTOCOL_VERSION,
			}
		end

		local function sendSyntheticRemoteFailure(
			item,
			code: string,
			message: string,
			retryable: boolean,
			details: any?
		)
			if item.responded then
				return
			end
			item.responded = true
			local remoteError = {
				code = code,
				message = message,
				retryable = retryable,
				details = details or { op = item.op },
			}
			wsSend(item.client, {
				type = "response",
				request_id = item.reqId,
				ok = false,
				error = remoteError,
				meta = remoteRequestMeta(item),
			})
			local preAudited = remoteState.preAuditedWrites and remoteState.preAuditedWrites[item.args] == true
			if preAudited then
				remoteState.preAuditedWrites[item.args] = nil
			end
			if item.isWrite and not preAudited and not item.auditHandled then
				postRemoteWriteLog({
					ts = os.time(),
					op = item.op,
					args = remoteAuditArgs(item.op, item.args),
					ok = false,
					synthetic = true,
					error = remoteError,
				})
				item.auditHandled = true
			end
		end

		local function finishRemoteRequest(item, ok: boolean, result: any)
			item.finished = true
			local late = item.responded == true
			if not late then
				item.responded = true
				if ok then
					wsSend(item.client, {
						type = "response",
						request_id = item.reqId,
						ok = true,
						value = result,
						meta = remoteRequestMeta(item),
					})
				else
					wsSend(item.client, {
						type = "response",
						request_id = item.reqId,
						ok = false,
						error = classifyRemoteHandlerError(item.op, result),
						meta = remoteRequestMeta(item),
					})
				end
			end
			local preAudited = remoteState.preAuditedWrites and remoteState.preAuditedWrites[item.args] == true
			if preAudited then
				remoteState.preAuditedWrites[item.args] = nil
				item.auditHandled = true
			end
			if item.isWrite and not item.auditHandled then
				local audit = {
					ts = os.time(),
					op = item.op,
					args = remoteAuditArgs(item.op, item.args),
					ok = ok,
					late = late,
				}
				if ok then
					local transformResult = remoteState.auditResultForOp
					if transformResult then
						local transformedOk, transformed = pcall(transformResult, item.op, result)
						audit.result = if transformedOk then transformed else "<result-redacted>"
					else
						audit.result = result
					end
				else
					audit.error = classifyRemoteHandlerError(item.op, result)
				end
				postRemoteWriteLog(audit)
				item.auditHandled = true
			end
		end

		local function ensureRemoteWriteWorker()
			if remoteWriteWorkerRunning then
				return
			end
			remoteWriteWorkerRunning = true
			task.spawn(runRemoteWriteWorker)
		end

		local function expireQueuedWrite(item, code: string, message: string)
			if item.started or item.finished then
				return
			end
			-- Name the handler that owns the lane. "write waited more than 10s" alone
			-- reads as "sync is broken"; "waiting behind playtest_request, running
			-- 47s" points straight at the culprit.
			local blocker = remoteWriteActiveItem
			if blocker ~= nil and blocker.executionStartedAt ~= nil then
				message = string.format(
					"%s (behind '%s', running %.0fs)",
					message,
					tostring(blocker.op),
					os.clock() - blocker.executionStartedAt
				)
			end
			-- Explicit rollback remains queued even after its caller receives a bounded
			-- wait response. Executing it late is safer than abandoning a recording.
			if not item.mustExecuteAfterTimeout then
				item.expired = true
			end
			sendSyntheticRemoteFailure(item, code, message, true, {
				op = item.op,
				queueLength = #remoteWriteQueue,
				queueLimit = REMOTE_WRITE_QUEUE_MAX,
			})
		end

		runRemoteWriteWorker = function()
			while #remoteWriteQueue > 0 do
				local item = table.remove(remoteWriteQueue, 1)
				if item.expired and not item.mustExecuteAfterTimeout then
					item.finished = true
					continue
				end
				-- Work submitted over a WebSocket session that has since gone away has
				-- nowhere to reply and no reason to run: replaying it (stale playtest
				-- run/stop requests, for instance) burns the lane for minutes while
				-- every live write behind it times out.
				if
					item.wsGeneration ~= nil
					and item.wsGeneration ~= getWsGeneration()
					and not item.mustExecuteAfterTimeout
				then
					item.expired = true
					sendSyntheticRemoteFailure(
						item,
						"SESSION_CLOSED",
						"queued write belonged to a closed plugin session and was dropped",
						true
					)
					item.finished = true
					staleWritesDropped += 1
					continue
				end

				item.started = true
				item.executionStartedAt = os.clock()
				remoteWriteActive = true
				remoteWriteActiveItem = item
				local responseBound = remoteWriteResponseBound(item)
				task.delay(responseBound, function()
					if item.started and not item.finished and not item.responded then
						item.executionTimedOut = true
						sendSyntheticRemoteFailure(
							item,
							"HANDLER_TIMEOUT",
							string.format(
								"write handler %s exceeded its %.0fs response bound and was abandoned; the write lane moved on",
								item.op,
								responseBound
							),
							true
						)
					end
				end)

				-- The handler runs on its own thread so a handler that never returns
				-- (a runtime request against a playtest that already ended, a Studio
				-- API that never calls back) cannot own the lane forever. Past the
				-- response bound the worker abandons it and continues; the orphan
				-- still reports its own late completion through finishRemoteRequest.
				local handlerSettled = false
				local laneReleased = Instance.new("BindableEvent")
				task.spawn(function()
					local ok, result = pcall(item.handler, item.args)
					handlerSettled = true
					finishRemoteRequest(item, ok, result)
					pcall(function()
						laneReleased:Fire()
					end)
				end)
				if not handlerSettled then
					task.delay(responseBound, function()
						pcall(function()
							laneReleased:Fire()
						end)
					end)
					laneReleased.Event:Wait()
				end
				laneReleased:Destroy()
				remoteWriteActive = false
				remoteWriteActiveItem = nil
				if not handlerSettled then
					item.abandoned = true
					dbg(
						string.format(
							"write lane abandoned '%s' after %.0fs; %d queued write(s) continue",
							item.op,
							responseBound,
							#remoteWriteQueue
						)
					)
				end

				if item.executionTimedOut or remoteWriteCleanupRequested then
					local cleanupReason = if item.executionTimedOut then "handler-timeout" else "disconnect"
					local cancelledCount, remainingCount = remoteState.requestRemoteWriteCleanup(cleanupReason)
					postRemoteWriteLog({
						ts = os.time(),
						op = "transaction_auto_cancel",
						ok = remainingCount == 0,
						reason = cleanupReason,
						cancelledCount = cancelledCount,
						remainingCount = remainingCount,
					})
					remoteWriteCleanupRequested = false
				end
			end
			remoteWriteWorkerRunning = false
			if staleWritesDropped > 0 then
				dbg(string.format("write lane dropped %d write(s) from closed session(s)", staleWritesDropped))
				staleWritesDropped = 0
			end
			if remoteWriteCleanupRequested and not remoteWriteActive then
				remoteState.cancelActiveWorkflowRecordings()
				remoteWriteCleanupRequested = false
			end
			if #remoteWriteQueue > 0 then
				ensureRemoteWriteWorker()
			end
		end

		local function enqueueRemoteWrite(item)
			item.isWrite = true
			item.wsGeneration = getWsGeneration()
			item.mustExecuteAfterTimeout = item.op == "transaction_finish"
				and tostring(item.args.action or "commit"):lower() == "cancel"
			if #remoteWriteQueue >= REMOTE_WRITE_QUEUE_MAX then
				if item.mustExecuteAfterTimeout then
					-- Preserve rollback by evicting the newest ordinary queued write.
					local evicted = nil
					for index = #remoteWriteQueue, 1, -1 do
						if not remoteWriteQueue[index].mustExecuteAfterTimeout then
							evicted = table.remove(remoteWriteQueue, index)
							break
						end
					end
					if evicted then
						expireQueuedWrite(evicted, "QUEUE_EVICTED", "write queue entry was evicted to admit rollback")
						evicted.finished = true
					end
				end
				if #remoteWriteQueue >= REMOTE_WRITE_QUEUE_MAX then
					sendSyntheticRemoteFailure(item, "BUSY", "remote write queue is full", true, {
						op = item.op,
						queueLength = #remoteWriteQueue,
						queueLimit = REMOTE_WRITE_QUEUE_MAX,
					})
					item.finished = true
					return
				end
			end
			table.insert(remoteWriteQueue, item)
			task.delay(REMOTE_WRITE_QUEUE_WAIT_SECONDS, function()
				expireQueuedWrite(
					item,
					"QUEUE_TIMEOUT",
					string.format("write waited more than %.0fs in the ordered queue", REMOTE_WRITE_QUEUE_WAIT_SECONDS)
				)
			end)
			ensureRemoteWriteWorker()
		end

		remoteState.requestRemoteWriteCleanup = function(reason: string?)
			local code = if reason == "handler-timeout" then "UPSTREAM_TIMEOUT" else "SESSION_CLOSED"
			local message = if reason == "handler-timeout"
				then "queued write was canceled because the active handler exceeded its response bound"
				else "plugin session closed before queued write execution"
			for _, item in ipairs(remoteWriteQueue) do
				if not item.started and not item.finished then
					item.expired = true
					sendSyntheticRemoteFailure(item, code, message, true)
				end
			end
			if remoteWriteActive then
				remoteWriteCleanupRequested = true
				return nil, nil
			else
				local cancelledCount, remainingCount = remoteState.cancelActiveWorkflowRecordings()
				remoteWriteCleanupRequested = false
				return cancelledCount, remainingCount
			end
		end

		remoteState.handleRemoteRequest = function(client, msg)
			local reqId = msg.request_id
			if type(reqId) ~= "number" then
				-- Without a request_id we have nothing to correlate a reply to — drop.
				log("ws: request missing numeric request_id")
				return
			end
			local op = tostring(msg.op or "")
			local args = msg.args
			if type(args) ~= "table" then
				args = {}
			end
			local handler = remoteHandlers[op]
			if not handler then
				wsSend(client, {
					type = "response",
					request_id = reqId,
					ok = false,
					error = {
						code = "UNKNOWN_OP",
						message = "unknown op: " .. op,
						retryable = false,
						details = { op = op },
					},
				})
				return
			end
			local receivedAt = os.clock()
			if op == "clipboard_copy" or op == "clipboard_paste" then
				-- Clipboard deadlines are end-to-end. The handler deducts time spent
				-- waiting behind another serialized write before it can mutate Studio.
				args.__rosyncReceivedAt = receivedAt
			end
			local item = {
				client = client,
				reqId = reqId,
				op = op,
				args = args,
				handler = handler,
				receivedAt = receivedAt,
				responded = false,
				started = false,
				finished = false,
				isWrite = REMOTE_WRITE_OPS[op] == true,
			}
			if item.isWrite and REMOTE_CONCURRENT_WRITE_OPS[op] ~= true then
				enqueueRemoteWrite(item)
			else
				item.started = true
				item.executionStartedAt = os.clock()
				local ok, result = pcall(handler, args)
				finishRemoteRequest(item, ok, result)
			end
		end
	end

	return { handlers = remoteHandlers, state = remoteState }
end

return RemoteControl
