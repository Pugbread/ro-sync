--!nonstrict
-- Ro Sync — Roblox Studio plugin.
-- Two-way sync between Studio DataModel and a local Rust daemon over HTTP.

local HttpService = game:GetService("HttpService")
local ChangeHistoryService = game:GetService("ChangeHistoryService")
local AssetService = game:GetService("AssetService")
local EncodingService = game:GetService("EncodingService")
local ScriptEditorService: any = nil
do
	local ok, svc = pcall(function()
		return game:GetService("ScriptEditorService")
	end)
	if ok then
		ScriptEditorService = svc
	end
end

local ECHO_GUARD_WINDOW = 0.1 -- seconds; mirrors Argon Core echo-suppression window

local WATCHED_SERVICES = {
	"ReplicatedStorage",
	"ServerScriptService",
	"StarterPlayer",
	"StarterGui",
	"Workspace",
	"ReplicatedFirst",
	"ServerStorage",
	"Lighting",
}

local SETTING_MANUAL_URL = "manualUrl"
local DEFAULT_URL = "http://127.0.0.1:7878"
local PORT_RANGE_START = 7878
local PORT_RANGE_END = 7890
local PROBE_TIMEOUT_SECONDS = 2
local MAX_LOG_LINES = 200
local SOURCE_WRITE_CONCURRENCY = 16
local RO_SYNC_LOGO_IMAGE = "rbxassetid://134480165435830"
local RO_SYNC_SETTINGS_IMAGE = "rbxassetid://128477924144442"

-- Identifier returned by `rosync version` / `ping`. Bump when breaking changes
-- to the remoteHandlers protocol ship.
local PLUGIN_VERSION_STRING = "2.1.0"
local PLUGIN_PROTOCOL_VERSION = 2

--------------------------------------------------------------------------------
-- Playtest runtime agent
--
-- Studio clones this plugin into PlayServer/PlayClient DataModels. Those
-- copies must not start the edit-mode UI or connect to the localhost daemon.
-- Instead, each one exposes a small, structured agent over
-- PluginConnectionService. The edit-mode copy is the sole external bridge.
--------------------------------------------------------------------------------

local function hostDataModelTypeName(): string
	-- HostDataModelType is not available in every Studio channel even though
	-- PluginConnectionService is. Prefer it when present, then use the
	-- connection direction plus RunService to distinguish the edit DataModel
	-- from its PlayServer/PlayClient clones.
	local name = "Unknown"
	pcall(function()
		name = plugin.HostDataModelType.Name
	end)
	if name == "Edit" or name == "PlayServer" or name == "PlayClient" then
		return name
	end

	local canConnectToEdit = false
	local canConnectToTest = false
	pcall(function()
		local connectionService = game:GetService("PluginConnectionService")
		canConnectToEdit = connectionService:CanHaveConnectionType(Enum.PluginConnectionTargetType.Edit)
		canConnectToTest = connectionService:CanHaveConnectionType(Enum.PluginConnectionTargetType.Test)
	end)
	if canConnectToEdit and not canConnectToTest then
		local runService = game:GetService("RunService")
		return if runService:IsServer() then "PlayServer" else "PlayClient"
	end
	if canConnectToTest and not canConnectToEdit then
		return "Edit"
	end

	local runService = game:GetService("RunService")
	if runService:IsRunning() then
		return if runService:IsServer() then "PlayServer" else "PlayClient"
	end
	return "Edit"
end

local function sha256Hex(value: string): string
	local digest: any = EncodingService:ComputeStringHash(value, Enum.HashAlgorithm.Sha256)
	if typeof(digest) == "buffer" then
		digest = buffer.tostring(digest)
	end
	local raw = tostring(digest)
	if #raw == 64 and string.match(raw, "^[0-9a-fA-F]+$") then
		return string.lower(raw)
	end
	local encoded = table.create(#raw)
	for index = 1, #raw do
		encoded[index] = string.format("%02x", string.byte(raw, index))
	end
	return table.concat(encoded)
end

local function runPlaytestRuntimeAgent()
	local runtimeId = HttpService:GenerateGUID(false)
	local role = if hostDataModelTypeName() == "PlayServer" then "server" else "client"
	local startedAt = os.clock()
	local runtimeLogs = {}
	local runtimeLogSeq = 1
	local runtimeCaptures = {}
	local runtimeCaptureCounter = 0
	local RUNTIME_CAPTURE_TTL_SECONDS = 120
	local RUNTIME_CAPTURE_MAX_SESSIONS = 4
	local RUNTIME_CAPTURE_MAX_AXIS = 16 * 1024
	local RUNTIME_CAPTURE_MAX_PIXELS = 64 * 1024 * 1024
	local RUNTIME_CAPTURE_MAX_BYTES = 128 * 1024 * 1024
	local RUNTIME_MAX_ENCODE_NODES = 5000
	local RUNTIME_MAX_TABLE_ENTRIES = 1000
	local RUNTIME_MAX_STRING_BYTES = 256 * 1024
	local RUNTIME_MAX_INPUT_ACTIONS = 200
	local RUNTIME_MAX_INPUT_SECONDS = 30
	local runtimeSessionToken = ""
	local runtimePlaytestJobId = ""
	pcall(function()
		local testArgs = game:GetService("StudioTestService"):GetTestArgs()
		if type(testArgs) == "table" then
			runtimeSessionToken = tostring(testArgs.__rosyncSessionToken or "")
			runtimePlaytestJobId = tostring(testArgs.__rosyncJobId or "")
		end
	end)
	if runtimeSessionToken == "" or runtimePlaytestJobId == "" then
		warn("[Ro Sync runtime] no authenticated Ro Sync playtest session; runtime bridge disabled")
		return
	end
	local seenRuntimeRequestIds = {}
	local function runtimeAuth(purpose: string): string
		return sha256Hex(runtimeSessionToken .. "|" .. purpose)
	end

	local function safeEncode(value: any, depth: number?, encodingState: any?): any
		local currentDepth = depth or 0
		local state = encodingState or { visited = {}, nodes = 0 }
		state.nodes += 1
		if state.nodes > RUNTIME_MAX_ENCODE_NODES then
			return "<node-budget-exceeded>"
		end
		if currentDepth > 8 then
			return "<max-depth>"
		end
		local kind = typeof(value)
		if kind == "nil" then
			return { __type = "Nil" }
		end
		if kind == "boolean" then
			return value
		end
		if kind == "number" then
			if value ~= value then
				return { __type = "Number", value = "NaN" }
			elseif value == math.huge then
				return { __type = "Number", value = "Infinity" }
			elseif value == -math.huge then
				return { __type = "Number", value = "-Infinity" }
			end
			return value
		end
		if kind == "string" then
			if #value > RUNTIME_MAX_STRING_BYTES then
				return string.sub(value, 1, RUNTIME_MAX_STRING_BYTES) .. "<truncated>"
			end
			return value
		end
		if kind == "Instance" then
			local names = {}
			local cursor = value
			while cursor and cursor ~= game do
				table.insert(names, 1, cursor.Name)
				cursor = cursor.Parent
			end
			return {
				__type = "Instance",
				path = table.concat(names, "/"),
				class = value.ClassName,
			}
		end
		if kind == "EnumItem" then
			return { __type = "EnumItem", enum = tostring(value.EnumType), name = value.Name, value = value.Value }
		end
		if kind == "Vector2" then
			return { __type = "Vector2", x = value.X, y = value.Y }
		end
		if kind == "Vector3" then
			return { __type = "Vector3", x = value.X, y = value.Y, z = value.Z }
		end
		if kind == "Color3" then
			return { __type = "Color3", r = value.R, g = value.G, b = value.B }
		end
		if kind == "CFrame" then
			return { __type = "CFrame", components = { value:GetComponents() } }
		end
		if kind == "table" then
			local visited = state.visited
			if visited[value] then
				return "<cycle>"
			end
			visited[value] = true

			local keys = {}
			local sequential = true
			local stringObject = true
			local maximumIndex = 0
			for key in pairs(value) do
				table.insert(keys, key)
				if type(key) ~= "string" then
					stringObject = false
				end
				if type(key) ~= "number" or key < 1 or key ~= math.floor(key) then
					sequential = false
				else
					maximumIndex = math.max(maximumIndex, key)
				end
				if #keys > RUNTIME_MAX_TABLE_ENTRIES then
					break
				end
			end
			local truncated = #keys > RUNTIME_MAX_TABLE_ENTRIES
			while #keys > RUNTIME_MAX_TABLE_ENTRIES do
				table.remove(keys)
			end
			sequential = sequential and not truncated and maximumIndex == #keys

			local out
			if sequential then
				out = table.create(#keys)
				for index = 1, #keys do
					out[index] = safeEncode(value[index], currentDepth + 1, state)
				end
			elseif stringObject then
				out = {}
				table.sort(keys)
				for _, key in ipairs(keys) do
					out[key] = safeEncode(value[key], currentDepth + 1, state)
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
						key = safeEncode(key, currentDepth + 1, state),
						value = safeEncode(value[key], currentDepth + 1, state),
					})
				end
			end
			visited[value] = nil
			return out
		end
		return tostring(value)
	end

	local playscriptRuntime = nil
	do
		local moduleScript = script:FindFirstChild("Playscript")
		if moduleScript and moduleScript:IsA("ModuleScript") then
			local okModule, module = pcall(require, moduleScript)
			if okModule and type(module) == "table" and type(module.newRuntime) == "function" then
				local okRuntime, runtime = pcall(module.newRuntime, {
					httpService = HttpService,
					encodingService = EncodingService,
					scriptEditorService = ScriptEditorService,
					sha256Hex = sha256Hex,
					runtimeId = runtimeId,
					jobId = runtimePlaytestJobId,
					sessionToken = runtimeSessionToken,
					role = role,
				})
				if okRuntime then
					playscriptRuntime = runtime
				else
					warn("[Ro Sync runtime] playscript initialization failed: " .. tostring(runtime))
				end
			else
				warn("[Ro Sync runtime] packaged playscript module is unavailable: " .. tostring(module))
			end
		end
	end

	local function send(connection, message): (boolean, string?)
		local ok, encoded = pcall(function()
			return HttpService:JSONEncode(message)
		end)
		if not ok then
			warn("[Ro Sync runtime] response encoding failed: " .. tostring(encoded))
			return false, tostring(encoded)
		end
		local sent, sendError = pcall(function()
			connection:SendMessage(encoded)
		end)
		if not sent then
			warn("[Ro Sync runtime] response send failed: " .. tostring(sendError))
			return false, tostring(sendError)
		end
		return true, nil
	end

	local function runtimeContext()
		local playerName = nil
		local userId = nil
		if role == "client" then
			pcall(function()
				local player = game:GetService("Players").LocalPlayer
				if player then
					playerName = player.Name
					userId = player.UserId
				end
			end)
		end
		return {
			runtimeId = runtimeId,
			playtestJobId = runtimePlaytestJobId,
			role = role,
			hostDataModelType = hostDataModelTypeName(),
			placeId = tostring(game.PlaceId),
			gameId = tostring(game.GameId),
			jobId = tostring(game.JobId),
			playerName = playerName,
			userId = userId,
			uptimeSeconds = os.clock() - startedAt,
		}
	end

	pcall(function()
		local LogService = game:GetService("LogService")
		LogService.MessageOut:Connect(function(message, messageType)
			local text = tostring(message)
			if #text > 16 * 1024 then
				text = string.sub(text, 1, 16 * 1024) .. "<truncated>"
			end
			local level = if messageType == Enum.MessageType.MessageError
				then "error"
				elseif messageType == Enum.MessageType.MessageWarning then "warn"
				else "info"
			table.insert(runtimeLogs, {
				seq = runtimeLogSeq,
				wall = os.time(),
				t = os.clock(),
				level = level,
				message = text,
			})
			runtimeLogSeq += 1
			if #runtimeLogs > 500 then
				table.remove(runtimeLogs, 1)
			end
			if playscriptRuntime then
				playscriptRuntime:onStudioLog(level, text)
			end
		end)
	end)

	local runtimeExecutionCounter = 0
	local function executeScriptSource(source: string, timeoutSeconds: number, identity: string)
		local parent
		local className
		if identity == "plugin" then
			parent = script
			className = "Script"
		elseif role == "server" then
			parent = game:GetService("ServerScriptService")
			className = "Script"
		else
			local player = game:GetService("Players").LocalPlayer
			if not player then
				error("LocalPlayer is not ready")
			end
			parent = player:FindFirstChildOfClass("PlayerScripts") or player:WaitForChild("PlayerScripts", 5)
			className = "LocalScript"
		end
		if not parent then
			error("runtime script parent is unavailable")
		end

		runtimeExecutionCounter += 1
		local holder = Instance.new("Folder")
		holder.Name = "__RoSyncRuntime_" .. runtimeId .. "_" .. tostring(runtimeExecutionCounter)
		local resultEvent = Instance.new("BindableEvent")
		resultEvent.Name = "Result"
		resultEvent.Parent = holder
		local runner = Instance.new(className)
		local anyRunner: any = runner
		runner.Name = "Runner"
		anyRunner.Disabled = true
		if identity == "plugin" then
			local contextOk, contextError = pcall(function()
				anyRunner.RunContext = Enum.RunContext.Plugin
			end)
			if not contextOk then
				holder:Destroy()
				error("plugin-identity execution is unavailable: " .. tostring(contextError))
			end
		end
		local wrapper = table.concat({
			'local __event = script.Parent:WaitForChild("Result")',
			"local function __rosync_run()",
			source,
			"end",
			"local __packed = table.pack(pcall(__rosync_run))",
			"__event:Fire(__packed)",
		}, "\n")
		local done = Instance.new("BindableEvent")
		local finished = false
		local packedResult = nil
		local function finish(packed)
			if not finished then
				finished = true
				packedResult = packed
				done:Fire()
			end
		end
		local runnerLogNeedle = holder.Name .. ".Runner"
		local compileLogConnection = game:GetService("LogService").MessageOut:Connect(function(message, messageType)
			if messageType ~= Enum.MessageType.MessageError then
				return
			end
			local messageText = tostring(message)
			if string.find(messageText, runnerLogNeedle, 1, true) ~= nil then
				finish(table.pack(false, "compile: " .. messageText))
			end
		end)
		local scriptErrorConnection = game:GetService("ScriptContext").Error
			:Connect(function(message, stack, errorSource)
				local messageText = tostring(message)
				local stackText = tostring(stack)
				if
					errorSource == runner
					or string.find(messageText, holder.Name, 1, true) ~= nil
					or string.find(stackText, holder.Name, 1, true) ~= nil
				then
					finish(table.pack(false, "compile: " .. messageText))
				end
			end)
		local resultConnection = resultEvent.Event:Connect(finish)
		local sourceOk, sourceError = pcall(function()
			anyRunner.Source = wrapper
		end)
		if not sourceOk and ScriptEditorService then
			holder.Parent = parent
			runner.Parent = holder
			sourceOk, sourceError = pcall(function()
				ScriptEditorService:UpdateSourceAsync(runner, function()
					return wrapper
				end)
			end)
		end
		if not sourceOk then
			resultConnection:Disconnect()
			compileLogConnection:Disconnect()
			scriptErrorConnection:Disconnect()
			done:Destroy()
			holder:Destroy()
			error("unable to install runtime script: " .. tostring(sourceError))
		end
		if holder.Parent == nil then
			holder.Parent = parent
		end
		if runner.Parent == nil then
			runner.Parent = holder
		end
		anyRunner.Disabled = false
		task.delay(timeoutSeconds, function()
			if not finished then
				finished = true
				done:Fire()
			end
		end)
		if not finished then
			done.Event:Wait()
		end
		resultConnection:Disconnect()
		compileLogConnection:Disconnect()
		scriptErrorConnection:Disconnect()
		done:Destroy()
		holder:Destroy()
		if packedResult == nil then
			error("runtime execution timed out after " .. tostring(timeoutSeconds) .. "s")
		end
		if not packedResult[1] then
			error("runtime: " .. tostring(packedResult[2]))
		end
		local values = {}
		local resultCount = (packedResult.n or #packedResult) - 1
		for index = 2, packedResult.n or #packedResult do
			values[index - 1] = safeEncode(packedResult[index])
		end
		return {
			count = resultCount,
			values = values,
			identity = identity,
			cooperativeTimeout = identity == "plugin",
		}
	end

	local function executePluginSource(source: string, timeoutSeconds: number)
		return executeScriptSource(source, timeoutSeconds, "plugin")
	end

	local function executeGameSource(source: string, timeoutSeconds: number)
		return executeScriptSource(source, timeoutSeconds, "game")
	end

	local function resolveRuntimePath(path: string): Instance?
		local cursor: Instance = game
		if path == "" or path == "game" then
			return cursor
		end
		for segment in string.gmatch(path, "[^/]+") do
			if segment ~= "game" and segment ~= "DataModel" then
				local nextInstance = cursor:FindFirstChild(segment)
				if not nextInstance then
					return nil
				end
				cursor = nextInstance
			end
		end
		return cursor
	end

	local function runtimePath(instance: Instance): string
		local names = {}
		local cursor: Instance? = instance
		while cursor and cursor ~= game do
			table.insert(names, 1, cursor.Name)
			cursor = cursor.Parent
		end
		return table.concat(names, "/")
	end

	local function resolveUiRoot(path: string): Instance?
		local normalized = path:gsub("^/+", ""):gsub("/+$", "")
		normalized = normalized:gsub("^game/", ""):gsub("^DataModel/", "")
		local first, remainder = normalized:match("^([^/]+)/?(.*)$")
		if not first then
			return nil
		end

		local root: Instance?
		local alias = first:lower()
		if alias == "playergui" then
			local player = game:GetService("Players").LocalPlayer
			root = if player then player:FindFirstChildOfClass("PlayerGui") else nil
		elseif alias == "coregui" then
			local ok, coreGui = pcall(function()
				return game:GetService("CoreGui")
			end)
			root = if ok then coreGui else nil
		else
			return resolveRuntimePath(path)
		end

		if not root or remainder == "" then
			return root
		end
		local cursor = root
		for segment in string.gmatch(remainder, "[^/]+") do
			local child = cursor:FindFirstChild(segment)
			if not child then
				return nil
			end
			cursor = child
		end
		return cursor
	end

	local function isEffectivelyVisible(instance: Instance): boolean
		local cursor: Instance? = instance
		while cursor and cursor ~= game do
			if cursor:IsA("GuiObject") and not cursor.Visible then
				return false
			end
			if cursor:IsA("LayerCollector") and not cursor.Enabled then
				return false
			end
			cursor = cursor.Parent
		end
		return true
	end

	local function inspectUi(args)
		if role ~= "client" then
			error("UI inspection requires a PlayClient context")
		end
		local roots = {}
		if type(args.root) == "string" and args.root ~= "" then
			local root = resolveUiRoot(args.root)
			if not root then
				error("UI root not found: " .. args.root)
			end
			table.insert(roots, root)
		else
			local player = game:GetService("Players").LocalPlayer
			if player then
				local playerGui = player:FindFirstChildOfClass("PlayerGui")
				if playerGui then
					table.insert(roots, playerGui)
				end
			end
			pcall(function()
				table.insert(roots, game:GetService("CoreGui"))
			end)
		end
		local limit = math.clamp(math.floor(tonumber(args.limit) or 1000), 1, 5000)
		local out = {}
		local function add(instance)
			if #out >= limit then
				return
			end
			if not (instance:IsA("GuiObject") or instance:IsA("LayerCollector")) then
				return
			end
			if args.class and tostring(args.class) ~= instance.ClassName then
				return
			end
			if args.name and not instance.Name:lower():find(tostring(args.name):lower(), 1, true) then
				return
			end
			local item = {
				path = runtimePath(instance),
				name = instance.Name,
				class = instance.ClassName,
				visible = isEffectivelyVisible(instance),
			}
			if instance:IsA("GuiObject") then
				item.localVisible = instance.Visible
				item.active = instance.Active
				item.position = { x = instance.AbsolutePosition.X, y = instance.AbsolutePosition.Y }
				item.size = { x = instance.AbsoluteSize.X, y = instance.AbsoluteSize.Y }
				item.zIndex = instance.ZIndex
			end
			pcall(function()
				item.enabled = (instance :: any).Enabled
			end)
			pcall(function()
				local text = tostring((instance :: any).Text)
				item.text = string.sub(text, 1, 500)
			end)
			table.insert(out, item)
		end
		for _, root in ipairs(roots) do
			add(root)
			for _, descendant in ipairs(root:GetDescendants()) do
				add(descendant)
				if #out >= limit then
					break
				end
			end
		end
		return { items = out, truncated = #out >= limit, count = #out }
	end

	local function sendInput(args)
		if role ~= "client" then
			error("virtual input requires a PlayClient context")
		end
		local inputOk, virtualInput = pcall(function()
			return (game:GetService("UserInputService") :: any):CreateVirtualInput()
		end)
		if not inputOk or virtualInput == nil then
			error("virtual input is unavailable in this PlayClient: " .. tostring(virtualInput))
		end
		local actions = if type(args.actions) == "table" then args.actions else { args }
		if #actions < 1 then
			error("input requires at least one action")
		end
		if #actions > RUNTIME_MAX_INPUT_ACTIONS then
			error("input action count exceeds " .. tostring(RUNTIME_MAX_INPUT_ACTIONS))
		end
		local timeoutSeconds = math.clamp(tonumber(args.__rosyncTimeoutSeconds) or 15, 0.1, 120)
		local durationBudget = math.min(timeoutSeconds, RUNTIME_MAX_INPUT_SECONDS)
		local plannedDurations = table.create(#actions)
		local plannedTotal = 0
		local function duration(value, defaultValue, fieldName)
			local seconds = tonumber(value)
			if seconds == nil then
				seconds = defaultValue
			end
			if seconds ~= seconds or seconds == math.huge or seconds == -math.huge or seconds < 0 then
				error(fieldName .. " must be a finite non-negative number")
			end
			if seconds > RUNTIME_MAX_INPUT_SECONDS then
				error(fieldName .. " exceeds the per-action duration limit")
			end
			return seconds
		end
		for index, action in ipairs(actions) do
			if type(action) ~= "table" then
				error("input action " .. tostring(index) .. " must be an object")
			end
			local kind = tostring(action.type or action.kind or "")
			local seconds = 0
			if kind == "key_press" or kind == "click" then
				seconds = duration(action.duration, 0.05, "input action duration")
			elseif kind == "wait" then
				seconds = duration(action.seconds or action.duration, 0, "input wait")
			elseif
				kind ~= "key"
				and kind ~= "mouse_move"
				and kind ~= "mouse_delta"
				and kind ~= "mouse_button"
				and kind ~= "text"
			then
				error("unknown input action: " .. kind)
			end
			plannedDurations[index] = seconds
			plannedTotal += seconds
		end
		if plannedTotal > durationBudget then
			error(
				"input sequence duration "
					.. tostring(plannedTotal)
					.. "s exceeds its "
					.. tostring(durationBudget)
					.. "s budget"
			)
		end
		local deadline = os.clock() + durationBudget
		local applied = 0
		for index, action in ipairs(actions) do
			if os.clock() >= deadline then
				error("input sequence exceeded its execution deadline")
			end
			local kind = tostring(action.type or action.kind or "")
			if kind == "key" then
				local keyCode = Enum.KeyCode[tostring(action.key or action.keyCode or "")]
				if not keyCode then
					error("unknown KeyCode: " .. tostring(action.key or action.keyCode))
				end
				virtualInput:SendKey(action.down ~= false, keyCode, action.repeatKey == true)
			elseif kind == "key_press" then
				local keyCode = Enum.KeyCode[tostring(action.key or action.keyCode or "")]
				if not keyCode then
					error("unknown KeyCode: " .. tostring(action.key or action.keyCode))
				end
				virtualInput:SendKey(true, keyCode, false)
				task.wait(plannedDurations[index])
				virtualInput:SendKey(false, keyCode, false)
			elseif kind == "mouse_move" then
				virtualInput:SendMousePosition(Vector2.new(tonumber(action.x) or 0, tonumber(action.y) or 0))
			elseif kind == "mouse_delta" then
				virtualInput:SendMouseDelta(Vector2.new(tonumber(action.x) or 0, tonumber(action.y) or 0))
			elseif kind == "mouse_button" or kind == "click" then
				local button = Enum.UserInputType[tostring(action.button or "MouseButton1")]
				if not button then
					error("unknown mouse button: " .. tostring(action.button))
				end
				local position = Vector2.new(tonumber(action.x) or 0, tonumber(action.y) or 0)
				if kind == "click" then
					virtualInput:SendMouseButton(position, button, true, 0)
					task.wait(plannedDurations[index])
					virtualInput:SendMouseButton(position, button, false, 0)
				else
					virtualInput:SendMouseButton(
						position,
						button,
						action.down ~= false,
						tonumber(action.repeatCount) or 0
					)
				end
			elseif kind == "text" then
				virtualInput:SendTextInput(tostring(action.text or ""))
			elseif kind == "wait" then
				task.wait(plannedDurations[index])
			end
			applied += 1
		end
		return { applied = applied }
	end

	local function destroyRuntimeCapture(session)
		if session then
			pcall(function()
				session.capture:Destroy()
			end)
		end
	end

	local function cleanupRuntimeCaptures()
		local now = os.clock()
		local active = {}
		for sessionId, session in pairs(runtimeCaptures) do
			if now - session.createdAt >= RUNTIME_CAPTURE_TTL_SECONDS then
				destroyRuntimeCapture(session)
				runtimeCaptures[sessionId] = nil
			else
				table.insert(active, { id = sessionId, createdAt = session.createdAt })
			end
		end
		if #active >= RUNTIME_CAPTURE_MAX_SESSIONS then
			table.sort(active, function(a, b)
				return a.createdAt < b.createdAt
			end)
			local oldest = active[1]
			destroyRuntimeCapture(runtimeCaptures[oldest.id])
			runtimeCaptures[oldest.id] = nil
		end
	end

	local function runtimeCapturePrepare(args)
		cleanupRuntimeCaptures()
		local service = game:GetService("StudioCaptureService")
		if not service:CanCaptureScreenshot() then
			error("screenshot permission is not granted; authorize capture in edit mode first")
		end
		local function vector(value, fieldName, allowNegative)
			if value == nil then
				return nil
			end
			if type(value) ~= "table" then
				error(fieldName .. " must be an object with x and y")
			end
			local x, y = tonumber(value.x), tonumber(value.y)
			if
				x == nil
				or y == nil
				or x ~= x
				or y ~= y
				or x == math.huge
				or y == math.huge
				or x == -math.huge
				or y == -math.huge
				or (not allowNegative and (x < 1 or y < 1))
			then
				error(fieldName .. " must contain finite numeric x and y")
			end
			if not allowNegative and (x > RUNTIME_CAPTURE_MAX_AXIS or y > RUNTIME_CAPTURE_MAX_AXIS) then
				error(fieldName .. " exceeds the per-axis capture limit")
			end
			return Vector2.new(math.floor(x), math.floor(y))
		end
		local position = vector(args.position, "position", true)
		local captureSize = vector(args.captureSize, "captureSize", false)
		local outputSize = vector(args.outputSize, "outputSize", false)
		if (position == nil) ~= (captureSize == nil) then
			error("position and captureSize must be provided together")
		end
		for _, size in pairs({ captureSize, outputSize }) do
			if size and size.X * size.Y > RUNTIME_CAPTURE_MAX_PIXELS then
				error("requested runtime capture exceeds the pixel limit")
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

		local capture = nil
		local bytes = nil
		local captureError = nil
		local okCapture, failure = pcall(function()
			capture = service:CaptureScreenshot(options)
			if capture == nil then
				error("CaptureScreenshot returned nil")
			end
			local deadline = os.clock() + math.min(30, tonumber(args.__rosyncTimeoutSeconds) or 30)
			while
				capture.BufferStatus == Enum.StudioCaptureBufferStatus.NotStarted
				or capture.BufferStatus == Enum.StudioCaptureBufferStatus.Pending
			do
				if os.clock() >= deadline then
					error("runtime screenshot timed out")
				end
				task.wait(0.02)
			end
			if capture.BufferStatus ~= Enum.StudioCaptureBufferStatus.Ready then
				local errors = {}
				pcall(function()
					for _, item in ipairs(capture:GetErrors()) do
						table.insert(errors, tostring(item))
					end
				end)
				error("runtime screenshot failed: " .. (#errors > 0 and table.concat(errors, "; ") or "unknown error"))
			end
			local resolution = capture.Resolution
			if
				resolution.X < 1
				or resolution.Y < 1
				or resolution.X > RUNTIME_CAPTURE_MAX_AXIS
				or resolution.Y > RUNTIME_CAPTURE_MAX_AXIS
				or resolution.X * resolution.Y > RUNTIME_CAPTURE_MAX_PIXELS
			then
				error("runtime screenshot returned invalid dimensions")
			end
			bytes = capture:GetBuffer()
			if typeof(bytes) ~= "buffer" then
				error("runtime screenshot did not return a buffer")
			end
			if buffer.len(bytes) < 1 or buffer.len(bytes) > RUNTIME_CAPTURE_MAX_BYTES then
				error("runtime screenshot exceeds the encoded-byte limit")
			end
		end)
		if not okCapture then
			captureError = tostring(failure)
			destroyRuntimeCapture({ capture = capture })
			error(captureError)
		end

		runtimeCaptureCounter += 1
		local sessionId = tostring(runtimeCaptureCounter) .. "-" .. HttpService:GenerateGUID(false)
		local session = { capture = capture, bytes = bytes, createdAt = os.clock() }
		runtimeCaptures[sessionId] = session
		task.delay(RUNTIME_CAPTURE_TTL_SECONDS, function()
			if runtimeCaptures[sessionId] == session then
				destroyRuntimeCapture(session)
				runtimeCaptures[sessionId] = nil
			end
		end)
		return {
			sessionId = sessionId,
			byteLength = buffer.len(bytes),
			width = math.floor(capture.Resolution.X),
			height = math.floor(capture.Resolution.Y),
			position = { x = capture.Position.X, y = capture.Position.Y },
		}
	end

	local function runtimeCaptureRead(args)
		cleanupRuntimeCaptures()
		local sessionId = tostring(args.sessionId or "")
		local session = runtimeCaptures[sessionId]
		if not session then
			error("runtime capture session not found: " .. sessionId)
		end
		local total = buffer.len(session.bytes)
		local offset = math.floor(tonumber(args.offset) or 0)
		if offset < 0 or offset >= total then
			error("runtime capture offset is outside the artifact")
		end
		local count =
			math.min(math.clamp(math.floor(tonumber(args.maxBytes) or 384 * 1024), 1, 512 * 1024), total - offset)
		local chunk = buffer.fromstring(buffer.readstring(session.bytes, offset, count))
		local encoded = game:GetService("EncodingService"):Base64Encode(chunk)
		if typeof(encoded) == "buffer" then
			encoded = buffer.tostring(encoded)
		end
		return {
			offset = offset,
			nextOffset = offset + count,
			eof = offset + count >= total,
			bytesBase64 = tostring(encoded),
		}
	end

	local function runtimeCaptureClose(args)
		local sessionId = tostring(args.sessionId or "")
		local session = runtimeCaptures[sessionId]
		if session then
			destroyRuntimeCapture(session)
			runtimeCaptures[sessionId] = nil
		end
		return true
	end

	local handlers = {}
	if playscriptRuntime then
		playscriptRuntime:registerHandlers(handlers)
	end
	function handlers.ping(_args)
		return runtimeContext()
	end
	function handlers.exec(args)
		local source = tostring(args.source or "")
		if source == "" then
			error("missing source")
		end
		local identity = tostring(args.identity or "game"):lower()
		local timeoutSeconds = math.clamp(tonumber(args.timeout) or 10, 0.1, 60)
		if identity == "plugin" then
			return executePluginSource(source, timeoutSeconds)
		end
		if identity ~= "game" then
			error("identity must be game or plugin")
		end
		return executeGameSource(source, timeoutSeconds)
	end
	function handlers.logs(args)
		local sinceSeq = tonumber(args.sinceSeq or args.since_seq) or 0
		local limit = math.clamp(math.floor(tonumber(args.limit) or 200), 1, 500)
		local entries = {}
		for _, entry in ipairs(runtimeLogs) do
			if entry.seq > sinceSeq then
				table.insert(entries, entry)
			end
		end
		while #entries > limit do
			table.remove(entries, 1)
		end
		return { entries = entries, nextSeq = runtimeLogSeq }
	end
	function handlers.ui_tree(args)
		return inspectUi(args)
	end
	function handlers.input(args)
		return sendInput(args)
	end
	function handlers.capture_prepare(args)
		return runtimeCapturePrepare(args)
	end
	function handlers.capture_read(args)
		return runtimeCaptureRead(args)
	end
	function handlers.capture_close(args)
		return runtimeCaptureClose(args)
	end
	function handlers.end_test(args)
		if role ~= "server" then
			error("ending a playtest requires the PlayServer context")
		end
		local result = if args.result == nil then "Ro Sync playtest stopped" else args.result
		-- Let the response reach the edit coordinator before EndTest tears down
		-- the PluginConnection and the temporary playtest DataModel.
		task.delay(0.1, function()
			local ok, failure = pcall(function()
				game:GetService("StudioTestService"):EndTest(result)
			end)
			if not ok then
				warn("[Ro Sync runtime] unable to end playtest: " .. tostring(failure))
			end
		end)
		return { requested = true }
	end

	local function handleConnection(connection)
		local okBound = pcall(function()
			connection:BindToMessage(function(payload)
				local decodeOk, message = pcall(function()
					return HttpService:JSONDecode(tostring(payload))
				end)
				if not decodeOk or type(message) ~= "table" then
					return
				end
				if message.kind == "rosync-playscript-frame" then
					if playscriptRuntime then
						playscriptRuntime:handleFrame(connection, message)
					end
					return
				end
				if message.kind == "rosync-runtime-discover" then
					local context = runtimeContext()
					context.kind = "rosync-runtime-hello"
					context.protocolVersion = PLUGIN_PROTOCOL_VERSION
					context.auth = runtimeAuth("hello|" .. runtimeId)
					send(connection, context)
					return
				end
				if message.kind ~= "rosync-runtime-request" then
					return
				end
				local requestId = tostring(message.requestId or "")
				local operation = tostring(message.op or "")
				if
					requestId == ""
					or seenRuntimeRequestIds[requestId]
					or tostring(message.auth or "") ~= runtimeAuth("request|" .. requestId .. "|" .. operation)
				then
					return
				end
				seenRuntimeRequestIds[requestId] = true
				local handler = handlers[operation]
				if not handler then
					send(connection, {
						kind = "rosync-runtime-response",
						requestId = requestId,
						auth = runtimeAuth("response|" .. requestId),
						ok = false,
						error = { code = "UNKNOWN_RUNTIME_OP", message = "unknown runtime op: " .. operation },
					})
					return
				end
				task.spawn(function()
					local callOk, result = pcall(handler, if type(message.args) == "table" then message.args else {})
					local response
					if callOk then
						response = {
							kind = "rosync-runtime-response",
							requestId = requestId,
							auth = runtimeAuth("response|" .. requestId),
							ok = true,
							value = safeEncode(result),
						}
					else
						response = {
							kind = "rosync-runtime-response",
							requestId = requestId,
							auth = runtimeAuth("response|" .. requestId),
							ok = false,
							error = { code = "RUNTIME_ERROR", message = tostring(result) },
						}
					end
					local sent, serializationError = send(connection, response)
					if not sent then
						send(connection, {
							kind = "rosync-runtime-response",
							requestId = requestId,
							auth = runtimeAuth("response|" .. requestId),
							ok = false,
							error = {
								code = "SERIALIZATION_ERROR",
								message = "runtime response could not be serialized: " .. tostring(serializationError),
							},
						})
					end
				end)
			end)
		end)
		if okBound then
			if playscriptRuntime then
				playscriptRuntime:attachConnection(connection)
			end
			local context = runtimeContext()
			context.kind = "rosync-runtime-hello"
			context.protocolVersion = PLUGIN_PROTOCOL_VERSION
			context.auth = runtimeAuth("hello|" .. runtimeId)
			send(connection, context)
		end
	end

	local ok, connectionService = pcall(function()
		return game:GetService("PluginConnectionService")
	end)
	if not ok or not connectionService then
		warn("[Ro Sync runtime] PluginConnectionService unavailable")
		return
	end
	pcall(function()
		for _, connection in ipairs(connectionService:GetPluginConnectionsOfType(Enum.PluginConnectionTargetType.Edit)) do
			handleConnection(connection)
		end
	end)
	connectionService.Connected:Connect(function(connection)
		if connection.Type == Enum.PluginConnectionTargetType.Edit then
			handleConnection(connection)
		end
	end)
end

local currentHostDataModelType = hostDataModelTypeName()
if currentHostDataModelType == "PlayServer" or currentHostDataModelType == "PlayClient" then
	runPlaytestRuntimeAgent()
	return
end

--------------------------------------------------------------------------------
-- LogService ring buffer
--
-- A single MessageOut subscription feeds a bounded ring buffer. `remoteHandlers.logs`
-- returns a filtered slice. `t` is os.clock() (monotonic, matches `ping.pong`),
-- `wall` is os.time() for HH:MM:SS rendering on the CLI side, `seq` is a
-- monotonic counter so `--tail` can page forward without duplicates.
--------------------------------------------------------------------------------

local LOG_RING_SIZE = 500
local logRing = table.create(LOG_RING_SIZE) -- 1..LOG_RING_SIZE cyclic slots
local logRingHead = 0 -- slot index of newest entry (0 = empty)
local logRingCount = 0
local logNextSeq = 1

local function mapLogLevel(messageType)
	if messageType == Enum.MessageType.MessageError then
		return "error"
	end
	if messageType == Enum.MessageType.MessageWarning then
		return "warn"
	end
	return "info"
end

local function logLevelRank(level)
	if level == "error" then
		return 2
	end
	if level == "warn" then
		return 1
	end
	return 0
end

local function appendLogEntry(message, messageType)
	local text = tostring(message)
	if #text > 16 * 1024 then
		text = string.sub(text, 1, 16 * 1024) .. "<truncated>"
	end
	local entry = {
		t = os.clock(),
		wall = os.time(),
		level = mapLogLevel(messageType),
		message = text,
		seq = logNextSeq,
	}
	logNextSeq += 1
	logRingHead += 1
	if logRingHead > LOG_RING_SIZE then
		logRingHead = 1
	end
	logRing[logRingHead] = entry
	if logRingCount < LOG_RING_SIZE then
		logRingCount += 1
	end
end

do
	local ok, err = pcall(function()
		local LogService = game:GetService("LogService")
		LogService.MessageOut:Connect(function(message, messageType)
			appendLogEntry(message, messageType)
		end)
		-- Seed the buffer with any history LogService has already accumulated
		-- so `rosync logs` works right after plugin reload.
		local okHist, history = pcall(function()
			return LogService:GetLogHistory()
		end)
		if okHist and typeof(history) == "table" then
			for _, entry in ipairs(history) do
				appendLogEntry(entry.message, entry.messageType)
			end
		end
	end)
	if not ok then
		warn("[Ro Sync] LogService subscription failed: " .. tostring(err))
	end
end

-- Return entries in oldest-to-newest order, filtered. `since_seconds` is
-- relative to os.clock() at call time; `since_seq` is absolute. `level_min`
-- drops entries below the given severity. `limit` caps the result (newest
-- wins when trimming).
local function readLogRing(sinceSeconds, sinceSeq, levelMin, limit)
	if logRingCount == 0 then
		return {}
	end
	local cutoffClock
	if typeof(sinceSeconds) == "number" and sinceSeconds >= 0 then
		cutoffClock = os.clock() - sinceSeconds
	end
	local seqCutoff = (typeof(sinceSeq) == "number") and sinceSeq or nil
	local levelCutoff = levelMin and logLevelRank(levelMin) or 0
	local start = logRingHead - logRingCount + 1
	if start < 1 then
		start += LOG_RING_SIZE
	end
	local collected = {}
	for i = 0, logRingCount - 1 do
		local idx = start + i
		if idx > LOG_RING_SIZE then
			idx -= LOG_RING_SIZE
		end
		local e = logRing[idx]
		if e then
			local pass = true
			if cutoffClock and e.t < cutoffClock then
				pass = false
			end
			if pass and seqCutoff and e.seq <= seqCutoff then
				pass = false
			end
			if pass and logLevelRank(e.level) < levelCutoff then
				pass = false
			end
			if pass then
				table.insert(collected, e)
			end
		end
	end
	if typeof(limit) == "number" and limit > 0 and #collected > limit then
		local trimmed = {}
		local startIdx = #collected - limit + 1
		for i = startIdx, #collected do
			table.insert(trimmed, collected[i])
		end
		return trimmed
	end
	return collected
end

--------------------------------------------------------------------------------
-- UI
--------------------------------------------------------------------------------

local toolbar = plugin:CreateToolbar("Ro Sync")
local toggleButton = toolbar:CreateButton("Ro Sync", "Open the Ro Sync panel", "")
toggleButton.ClickableWhenViewportHidden = true

local widgetInfo = DockWidgetPluginGuiInfo.new(Enum.InitialDockState.Right, false, false, 360, 300, 280, 220)
local widget = plugin:CreateDockWidgetPluginGuiAsync("RoSyncPanel", widgetInfo)
widget.Title = "Ro Sync"
widget.Name = "RoSyncPanel"

toggleButton.Click:Connect(function()
	widget.Enabled = not widget.Enabled
end)

local startConnect: (() -> ())? = nil

local function trimUiUrl(s)
	local trimmed = tostring(s or ""):match("^%s*(.-)%s*$") or ""
	return (trimmed:gsub("/+$", ""))
end

local function mountFallbackUi(reason)
	local root = Instance.new("ScrollingFrame")
	root.Size = UDim2.fromScale(1, 1)
	root.BackgroundColor3 = Color3.fromRGB(25, 26, 31)
	root.BorderSizePixel = 0
	root.AutomaticCanvasSize = Enum.AutomaticSize.Y
	root.CanvasSize = UDim2.fromScale(0, 0)
	root.ScrollBarImageColor3 = Color3.fromRGB(119, 125, 134)
	root.ScrollBarThickness = 4
	root.ScrollingDirection = Enum.ScrollingDirection.Y
	root.Parent = widget

	local pad = Instance.new("UIPadding")
	pad.PaddingTop = UDim.new(0, 18)
	pad.PaddingBottom = UDim.new(0, 18)
	pad.PaddingLeft = UDim.new(0, 16)
	pad.PaddingRight = UDim.new(0, 16)
	pad.Parent = root

	local layout = Instance.new("UIListLayout")
	layout.SortOrder = Enum.SortOrder.LayoutOrder
	layout.Padding = UDim.new(0, 12)
	layout.Parent = root

	local logo = Instance.new("ImageLabel")
	logo.LayoutOrder = 1
	logo.Size = UDim2.fromOffset(113, 18)
	logo.BackgroundTransparency = 1
	logo.Image = RO_SYNC_LOGO_IMAGE
	logo.ImageColor3 = Color3.fromRGB(65, 135, 245)
	logo.ScaleType = Enum.ScaleType.Fit
	logo.Parent = root

	local loadError = Instance.new("TextLabel")
	loadError.LayoutOrder = 2
	loadError.Size = UDim2.new(1, 0, 0, 42)
	loadError.BackgroundColor3 = Color3.fromRGB(43, 101, 199)
	loadError.BorderSizePixel = 0
	loadError.TextColor3 = Color3.fromRGB(244, 245, 247)
	loadError.Font = Enum.Font.SourceSansBold
	loadError.TextSize = 13
	loadError.TextWrapped = true
	loadError.Text = "React UI bundle unavailable; running fallback UI. " .. tostring(reason or "")
	loadError.Parent = root

	local loadCorner = Instance.new("UICorner")
	loadCorner.CornerRadius = UDim.new(0, 6)
	loadCorner.Parent = loadError

	local connectionCard = Instance.new("Frame")
	connectionCard.LayoutOrder = 3
	connectionCard.Size = UDim2.new(1, 0, 0, 70)
	connectionCard.BackgroundColor3 = Color3.fromRGB(35, 37, 40)
	connectionCard.BorderSizePixel = 0
	connectionCard.Parent = root

	local connectionCorner = Instance.new("UICorner")
	connectionCorner.CornerRadius = UDim.new(0, 7)
	connectionCorner.Parent = connectionCard

	local connectionStroke = Instance.new("UIStroke")
	connectionStroke.Color = Color3.fromRGB(255, 255, 255)
	connectionStroke.Transparency = 0.88
	connectionStroke.Thickness = 1
	connectionStroke.Parent = connectionCard

	local placeName = Instance.new("TextLabel")
	placeName.Position = UDim2.fromOffset(14, 9)
	placeName.Size = UDim2.new(1, -46, 0, 23)
	placeName.BackgroundTransparency = 1
	placeName.Font = Enum.Font.SourceSansBold
	placeName.Text = game.Name
	placeName.TextColor3 = Color3.fromRGB(244, 245, 247)
	placeName.TextSize = 17
	placeName.TextTruncate = Enum.TextTruncate.AtEnd
	placeName.TextXAlignment = Enum.TextXAlignment.Left
	placeName.Parent = connectionCard

	local status = Instance.new("TextLabel")
	status.Position = UDim2.fromOffset(14, 35)
	status.Size = UDim2.new(1, -28, 0, 19)
	status.BackgroundTransparency = 1
	status.Font = Enum.Font.Code
	status.Text = "Auto-discover localhost:7878–7890"
	status.TextColor3 = Color3.fromRGB(119, 125, 134)
	status.TextSize = 13
	status.TextTruncate = Enum.TextTruncate.AtEnd
	status.TextXAlignment = Enum.TextXAlignment.Left
	status.Parent = connectionCard

	local stateDot = Instance.new("Frame")
	stateDot.AnchorPoint = Vector2.new(1, 0)
	stateDot.Position = UDim2.new(1, -14, 0, 16)
	stateDot.Size = UDim2.fromOffset(8, 8)
	stateDot.BackgroundColor3 = Color3.fromRGB(119, 125, 134)
	stateDot.BorderSizePixel = 0
	stateDot.Parent = connectionCard

	local dotCorner = Instance.new("UICorner")
	dotCorner.CornerRadius = UDim.new(1, 0)
	dotCorner.Parent = stateDot

	local row = Instance.new("Frame")
	row.LayoutOrder = 4
	row.Size = UDim2.new(1, 0, 0, 40)
	row.BackgroundTransparency = 1
	row.Parent = root

	local rowLayout = Instance.new("UIListLayout")
	rowLayout.FillDirection = Enum.FillDirection.Horizontal
	rowLayout.HorizontalAlignment = Enum.HorizontalAlignment.Right
	rowLayout.SortOrder = Enum.SortOrder.LayoutOrder
	rowLayout.Padding = UDim.new(0, 10)
	rowLayout.Parent = row

	local settingsBtn = Instance.new("TextButton")
	settingsBtn.LayoutOrder = 1
	settingsBtn.Size = UDim2.fromOffset(40, 40)
	settingsBtn.BackgroundColor3 = Color3.fromRGB(35, 37, 40)
	settingsBtn.BorderSizePixel = 0
	settingsBtn.TextColor3 = Color3.fromRGB(244, 245, 247)
	settingsBtn.Font = Enum.Font.SourceSansBold
	settingsBtn.TextSize = 15
	settingsBtn.Text = ""
	settingsBtn.Parent = row

	local settingsCorner = Instance.new("UICorner")
	settingsCorner.CornerRadius = UDim.new(0, 6)
	settingsCorner.Parent = settingsBtn

	local settingsIcon = Instance.new("ImageLabel")
	settingsIcon.AnchorPoint = Vector2.new(0.5, 0.5)
	settingsIcon.Position = UDim2.fromScale(0.5, 0.5)
	settingsIcon.Size = UDim2.fromOffset(32, 32)
	settingsIcon.BackgroundTransparency = 1
	settingsIcon.Image = RO_SYNC_SETTINGS_IMAGE
	settingsIcon.ImageColor3 = Color3.fromRGB(244, 245, 247)
	settingsIcon.ScaleType = Enum.ScaleType.Fit
	settingsIcon.Parent = settingsBtn

	local connectBtn = Instance.new("TextButton")
	connectBtn.LayoutOrder = 2
	connectBtn.Size = UDim2.new(0, 122, 1, 0)
	connectBtn.BackgroundColor3 = Color3.fromRGB(65, 135, 245)
	connectBtn.BorderSizePixel = 0
	connectBtn.TextColor3 = Color3.fromRGB(255, 255, 255)
	connectBtn.Font = Enum.Font.SourceSansBold
	connectBtn.TextSize = 15
	connectBtn.Text = "Connect"
	connectBtn.Parent = row

	local btnCorner = Instance.new("UICorner")
	btnCorner.CornerRadius = UDim.new(0, 6)
	btnCorner.Parent = connectBtn

	local banner = Instance.new("TextLabel")
	banner.LayoutOrder = 5
	banner.Size = UDim2.new(1, 0, 0, 42)
	banner.BackgroundColor3 = Color3.fromRGB(43, 101, 199)
	banner.BorderSizePixel = 0
	banner.TextColor3 = Color3.fromRGB(244, 245, 247)
	banner.Font = Enum.Font.SourceSansBold
	banner.TextSize = 13
	banner.TextWrapped = true
	banner.Visible = false
	banner.Parent = root

	local bannerCorner = Instance.new("UICorner")
	bannerCorner.CornerRadius = UDim.new(0, 6)
	bannerCorner.Parent = banner

	local settingsFrame = Instance.new("Frame")
	settingsFrame.LayoutOrder = 6
	settingsFrame.Size = UDim2.new(1, 0, 0, 216)
	settingsFrame.BackgroundTransparency = 1
	settingsFrame.Visible = false
	settingsFrame.Parent = root

	local settingsLayout = Instance.new("UIListLayout")
	settingsLayout.SortOrder = Enum.SortOrder.LayoutOrder
	settingsLayout.Padding = UDim.new(0, 8)
	settingsLayout.Parent = settingsFrame

	local urlLabel = Instance.new("TextLabel")
	urlLabel.LayoutOrder = 1
	urlLabel.Size = UDim2.new(1, 0, 0, 16)
	urlLabel.BackgroundTransparency = 1
	urlLabel.Font = Enum.Font.SourceSans
	urlLabel.Text = "Manual URL override"
	urlLabel.TextColor3 = Color3.fromRGB(171, 175, 182)
	urlLabel.TextSize = 13
	urlLabel.TextXAlignment = Enum.TextXAlignment.Left
	urlLabel.Parent = settingsFrame

	local urlBox = Instance.new("TextBox")
	urlBox.LayoutOrder = 2
	urlBox.Size = UDim2.new(1, 0, 0, 40)
	urlBox.BackgroundColor3 = Color3.fromRGB(29, 31, 34)
	urlBox.BorderSizePixel = 0
	urlBox.ClearTextOnFocus = false
	urlBox.Font = Enum.Font.Code
	urlBox.PlaceholderText = DEFAULT_URL
	urlBox.Text = tostring(plugin:GetSetting(SETTING_MANUAL_URL) or "")
	urlBox.PlaceholderColor3 = Color3.fromRGB(119, 125, 134)
	urlBox.TextColor3 = Color3.fromRGB(244, 245, 247)
	urlBox.TextSize = 13
	urlBox.TextXAlignment = Enum.TextXAlignment.Left
	urlBox.Parent = settingsFrame

	local urlCorner = Instance.new("UICorner")
	urlCorner.CornerRadius = UDim.new(0, 6)
	urlCorner.Parent = urlBox

	local urlStroke = Instance.new("UIStroke")
	urlStroke.Color = Color3.fromRGB(255, 255, 255)
	urlStroke.Transparency = 0.9
	urlStroke.Thickness = 1
	urlStroke.Parent = urlBox

	local urlPad = Instance.new("UIPadding")
	urlPad.PaddingLeft = UDim.new(0, 12)
	urlPad.PaddingRight = UDim.new(0, 12)
	urlPad.Parent = urlBox

	local logLabel = Instance.new("TextLabel")
	logLabel.LayoutOrder = 3
	logLabel.Size = UDim2.new(1, 0, 0, 18)
	logLabel.BackgroundTransparency = 1
	logLabel.Font = Enum.Font.SourceSans
	logLabel.Text = "Activity"
	logLabel.TextColor3 = Color3.fromRGB(171, 175, 182)
	logLabel.TextSize = 13
	logLabel.TextXAlignment = Enum.TextXAlignment.Left
	logLabel.Parent = settingsFrame

	local logFrame = Instance.new("ScrollingFrame")
	logFrame.LayoutOrder = 4
	logFrame.Size = UDim2.new(1, 0, 0, 118)
	logFrame.BackgroundColor3 = Color3.fromRGB(29, 31, 34)
	logFrame.BorderSizePixel = 0
	logFrame.AutomaticCanvasSize = Enum.AutomaticSize.Y
	logFrame.CanvasSize = UDim2.fromScale(0, 0)
	logFrame.ScrollBarThickness = 6
	logFrame.ScrollBarImageColor3 = Color3.fromRGB(119, 125, 134)
	logFrame.Parent = settingsFrame

	local logCorner = Instance.new("UICorner")
	logCorner.CornerRadius = UDim.new(0, 6)
	logCorner.Parent = logFrame

	local logStroke = Instance.new("UIStroke")
	logStroke.Color = Color3.fromRGB(255, 255, 255)
	logStroke.Transparency = 0.92
	logStroke.Thickness = 1
	logStroke.Parent = logFrame

	local logLayout = Instance.new("UIListLayout")
	logLayout.SortOrder = Enum.SortOrder.LayoutOrder
	logLayout.Padding = UDim.new(0, 3)
	logLayout.Parent = logFrame

	local logPad = Instance.new("UIPadding")
	logPad.PaddingTop = UDim.new(0, 8)
	logPad.PaddingBottom = UDim.new(0, 8)
	logPad.PaddingLeft = UDim.new(0, 10)
	logPad.PaddingRight = UDim.new(0, 10)
	logPad.Parent = logFrame

	local logCount = 0

	connectBtn.Activated:Connect(function()
		if startConnect then
			startConnect()
		end
	end)

	settingsBtn.Activated:Connect(function()
		settingsFrame.Visible = not settingsFrame.Visible
	end)

	urlBox.FocusLost:Connect(function()
		local normalized = trimUiUrl(urlBox.Text)
		urlBox.Text = normalized
		plugin:SetSetting(SETTING_MANUAL_URL, normalized)
	end)

	local controller = {}

	function controller.getManualUrl()
		return urlBox.Text or ""
	end

	function controller.setManualUrl(text)
		urlBox.Text = tostring(text or "")
	end

	function controller.setProjectName(text)
		placeName.Text = tostring(text or placeName.Text)
	end

	function controller.setStatus(state, detail)
		if state == "idle" then
			status.Text = "Auto-discover localhost:7878–7890"
			status.TextColor3 = Color3.fromRGB(119, 125, 134)
			stateDot.BackgroundColor3 = Color3.fromRGB(119, 125, 134)
			connectBtn.Text = "Connect"
		elseif state == "probing" then
			status.Text = "Scanning localhost..."
			status.TextColor3 = Color3.fromRGB(65, 135, 245)
			stateDot.BackgroundColor3 = Color3.fromRGB(65, 135, 245)
			connectBtn.Text = "Cancel"
		elseif state == "connecting" then
			status.Text = "Connecting..."
			status.TextColor3 = Color3.fromRGB(65, 135, 245)
			stateDot.BackgroundColor3 = Color3.fromRGB(65, 135, 245)
			connectBtn.Text = "Cancel"
		elseif state == "waiting" then
			status.Text = "Waiting for initial sync..."
			status.TextColor3 = Color3.fromRGB(65, 135, 245)
			stateDot.BackgroundColor3 = Color3.fromRGB(65, 135, 245)
			connectBtn.Text = "Cancel"
		elseif state == "project_init" then
			status.Text = "New Studio project · ready to create"
			status.TextColor3 = Color3.fromRGB(65, 135, 245)
			stateDot.BackgroundColor3 = Color3.fromRGB(65, 135, 245)
			connectBtn.Text = "Create Project"
		elseif state == "project_creating" then
			status.Text = "Creating Ro Sync project..."
			status.TextColor3 = Color3.fromRGB(65, 135, 245)
			stateDot.BackgroundColor3 = Color3.fromRGB(65, 135, 245)
			connectBtn.Text = "Cancel"
		elseif state == "project_waiting" then
			status.Text = "Project ready · waiting for Desktop..."
			status.TextColor3 = Color3.fromRGB(65, 135, 245)
			stateDot.BackgroundColor3 = Color3.fromRGB(65, 135, 245)
			connectBtn.Text = "Cancel"
		elseif state == "connected" then
			status.Text = string.format("localhost:%s", tostring(detail or "?"))
			status.TextColor3 = Color3.fromRGB(171, 175, 182)
			stateDot.BackgroundColor3 = Color3.fromRGB(74, 193, 123)
			connectBtn.Text = "Disconnect"
		elseif state == "no_daemon" then
			status.Text = string.format("No daemon for GameId %s", tostring(detail or "?"))
			status.TextColor3 = Color3.fromRGB(207, 72, 72)
			stateDot.BackgroundColor3 = Color3.fromRGB(207, 72, 72)
			connectBtn.Text = "Connect"
		elseif state == "wrong_game" then
			status.Text = "Daemon belongs to another game"
			status.TextColor3 = Color3.fromRGB(207, 72, 72)
			stateDot.BackgroundColor3 = Color3.fromRGB(207, 72, 72)
			connectBtn.Text = "Connect"
		elseif state == "error" then
			status.Text = tostring(detail or "Connection error")
			status.TextColor3 = Color3.fromRGB(207, 72, 72)
			stateDot.BackgroundColor3 = Color3.fromRGB(207, 72, 72)
			connectBtn.Text = "Connect"
		else
			status.Text = tostring(state or "Ready")
			status.TextColor3 = Color3.fromRGB(119, 125, 134)
			stateDot.BackgroundColor3 = Color3.fromRGB(119, 125, 134)
		end
	end

	function controller.setBanner(text)
		local nextText = tostring(text or "")
		banner.Text = nextText
		banner.Visible = nextText ~= ""
	end

	function controller.addLog(msg)
		logCount += 1
		local line = Instance.new("TextLabel")
		line.LayoutOrder = logCount
		line.Size = UDim2.new(1, -20, 0, 16)
		line.BackgroundTransparency = 1
		line.TextColor3 = Color3.fromRGB(164, 170, 176)
		line.Font = Enum.Font.Code
		line.TextSize = 12
		line.TextXAlignment = Enum.TextXAlignment.Left
		line.TextTruncate = Enum.TextTruncate.AtEnd
		line.Text = string.format("[%.1fs] %s", os.clock(), tostring(msg))
		line.Parent = logFrame
		for _, child in ipairs(logFrame:GetChildren()) do
			if child:IsA("TextLabel") and child.LayoutOrder <= logCount - MAX_LOG_LINES then
				child:Destroy()
			end
		end
		logFrame.CanvasPosition = Vector2.new(0, math.huge)
	end

	function controller.showUnpublishedModal(onResult)
		settingsFrame.Visible = true
		controller.setBanner("Publish your place first, or use the manual URL field before connecting.")
		if onResult then
			onResult(nil)
		end
	end

	function controller.hideUnpublishedModal(result)
		return result
	end

	function controller.destroy()
		root:Destroy()
	end

	return controller
end

local ui
local appOk, appOrErr = pcall(function()
	local appModule = (script.Parent :: Instance):WaitForChild("App", 2)
	if not appModule then
		error("missing App ModuleScript")
	end
	return require(appModule)
end)

if appOk and type(appOrErr) == "table" and type(appOrErr.mount) == "function" then
	ui = appOrErr.mount(widget, {
		initialUrl = tostring(plugin:GetSetting(SETTING_MANUAL_URL) or ""),
		defaultUrl = DEFAULT_URL,
		logoImage = RO_SYNC_LOGO_IMAGE,
		placeName = game.Name,
		getStoredManualUrl = function()
			return plugin:GetSetting(SETTING_MANUAL_URL)
		end,
		onConnect = function()
			if startConnect then
				startConnect()
			end
		end,
		onUrlCommit = function(text)
			plugin:SetSetting(SETTING_MANUAL_URL, trimUiUrl(text))
		end,
	})
else
	ui = mountFallbackUi(appOrErr)
end

local urlBox = setmetatable({}, {
	__index = function(_, key)
		if key == "Text" then
			return ui.getManualUrl()
		end
		return nil
	end,
	__newindex = function(proxy, key, value)
		if key == "Text" then
			ui.setManualUrl(value)
		else
			rawset(proxy, key, value)
		end
	end,
})

local function log(msg)
	ui.addLog(msg)
end

local function setPill(state, detail)
	ui.setStatus(state, detail)
end

local function setBanner(text)
	ui.setBanner(text)
end

local function showUnpublishedModal(onResult: (string?) -> ())
	ui.showUnpublishedModal(onResult)
end

--------------------------------------------------------------------------------
-- Serialization
--------------------------------------------------------------------------------

-- Classes we never serialize, push, or apply. Camera lives under Workspace and
-- mutates on every viewport movement in Studio (CFrame / Focus / FieldOfView),
-- which would spam the daemon with ops. The user's viewport state isn't data
-- they want synced to disk.
local SUPPRESS_CLASSES = {
	["Camera"] = true,
	["Terrain"] = true, -- huge, binary voxel data; skip entirely
	["PlayerScripts"] = true, -- runtime-only, per-player
	["PackageLink"] = true, -- Roblox package bookkeeping
}

local function safeGet(inst, prop)
	local ok, val = pcall(function()
		return inst[prop]
	end)
	if ok then
		return val
	end
	return nil
end

-- The four Roblox classes that round-trip to/from disk. Every other class is
-- Studio-authoritative via the daemon's live tree cache — we never
-- Instance.new them, never apply property ops to them, never push their
-- changes to disk.
local ALLOWED_CLASSES = {
	Folder = true,
	Script = true,
	LocalScript = true,
	ModuleScript = true,
}

local SCRIPT_CLASSES = {
	Script = true,
	LocalScript = true,
	ModuleScript = true,
}

local AVOID_SYNC_ATTRIBUTE = "AvoidSync"

local function hasAvoidSyncAttribute(inst): boolean
	local ok, value = pcall(function()
		return inst:GetAttribute(AVOID_SYNC_ATTRIBUTE)
	end)
	return ok and value == true
end

local function isAvoidSyncBlocked(inst): boolean
	local cur = inst
	while cur and cur ~= game do
		if hasAvoidSyncAttribute(cur) then
			return true
		end
		cur = cur.Parent
	end
	return false
end

-- Pass-through rule: any non-ALLOWED instance (Part, Model, Tool, ValueBase,
-- etc.) is materialized on disk as a plain directory whenever it has at least
-- one ALLOWED descendant. The real class stays in the live tree cache; on
-- disk the pathway is just `Name/`. This lets users place a script anywhere
-- inside Studio and still have it round-trip to a file.
local isSyncable
local function hasSyncableDescendant(inst)
	if isAvoidSyncBlocked(inst) then
		return false
	end
	for _, child in ipairs(inst:GetChildren()) do
		if not SUPPRESS_CLASSES[child.ClassName] and isSyncable(child) then
			return true
		end
	end
	return false
end

-- True if an instance needs a presence on disk — either it's directly ALLOWED,
-- or it's a pass-through container that has at least one ALLOWED descendant.
isSyncable = function(inst)
	if isAvoidSyncBlocked(inst) then
		return false
	end
	if inst:IsA("LuaSourceContainer") then
		return true
	end
	if inst.ClassName == "Folder" then
		return hasSyncableDescendant(inst)
	end
	return hasSyncableDescendant(inst)
end

-- Container instances we've materialized on disk: pass-through non-ALLOWED
-- instances and synced Folders with script descendants. Tracked here so that
-- `onDescendantRemoving` can still emit a cleanup delete after Destroy() has
-- already removed the children that made the container syncable.
-- Declared up-front because `serializeInstance` writes to it as a side
-- effect during /snapshot bootstrap.
local materializedContainers: { [Instance]: true } = {}

local function readScriptSource(inst): string
	if ScriptEditorService then
		local ok, src = pcall(function()
			return ScriptEditorService:GetEditorSource(inst)
		end)
		if ok and type(src) == "string" then
			return src
		end
	end
	return safeGet(inst, "Source") or ""
end

-- Post-scope-down: only Source (for scripts) round-trips. Every other property
-- is Studio-authoritative — we never serialize it, never push it, never apply
-- it. Non-script nodes get `properties:{}` purely for schema stability so the
-- daemon's op deserializer never sees a missing field.
local function serializeProperties(inst)
	if inst:IsA("LuaSourceContainer") then
		return { Source = readScriptSource(inst) }
	end
	return {}
end

-- Serialize a subtree for /push and /snapshot. Non-ALLOWED classes are
-- emitted as pass-through Folders if (and only if) they contain an ALLOWED
-- descendant — their real class stays Studio-authoritative in the live tree.
-- Callers should pre-check `isSyncable(inst)` before recursing; this function
-- does not re-check because the top-level `inst` is always syncable by the
-- time a caller has decided to emit an op for it.
--
-- Side effect: every non-ALLOWED instance emitted as a pass-through Folder is
-- recorded in `materializedContainers` so that a subsequent delete on the
-- container still triggers a disk cleanup, even after its ALLOWED children
-- have already fired their own Removing events.
local serializeInstance
serializeInstance = function(inst)
	if isAvoidSyncBlocked(inst) then
		return nil
	end
	local allowed = ALLOWED_CLASSES[inst.ClassName] == true
	if not allowed and not SUPPRESS_CLASSES[inst.ClassName] then
		materializedContainers[inst] = true
	end
	local node = {
		class = allowed and inst.ClassName or "Folder",
		name = inst.Name,
		properties = allowed and serializeProperties(inst) or {},
		children = {},
	}
	for _, child in ipairs(inst:GetChildren()) do
		if not SUPPRESS_CLASSES[child.ClassName] and not isAvoidSyncBlocked(child) and isSyncable(child) then
			local childNode = serializeInstance(child)
			if childNode then
				table.insert(node.children, childNode)
			end
		end
	end
	if allowed and inst.ClassName == "Folder" and #node.children == 0 then
		return nil
	end
	if inst.ClassName == "Folder" then
		materializedContainers[inst] = true
	end
	return node
end

local function avoidSyncMarker(inst)
	local allowed = ALLOWED_CLASSES[inst.ClassName] == true
	return {
		class = allowed and inst.ClassName or "Folder",
		name = inst.Name,
		properties = {},
		children = {},
		avoidSync = true,
	}
end

local serializeSnapshotInstance
serializeSnapshotInstance = function(inst)
	if hasAvoidSyncAttribute(inst) then
		return avoidSyncMarker(inst)
	end
	if isAvoidSyncBlocked(inst) then
		return nil
	end
	local allowed = ALLOWED_CLASSES[inst.ClassName] == true
	if not allowed and not SUPPRESS_CLASSES[inst.ClassName] then
		materializedContainers[inst] = true
	end
	local node = {
		class = allowed and inst.ClassName or "Folder",
		name = inst.Name,
		properties = allowed and serializeProperties(inst) or {},
		children = {},
	}
	for _, child in ipairs(inst:GetChildren()) do
		if not SUPPRESS_CLASSES[child.ClassName] and (hasAvoidSyncAttribute(child) or isSyncable(child)) then
			local childNode = serializeSnapshotInstance(child)
			if childNode then
				table.insert(node.children, childNode)
			end
		end
	end
	if allowed and inst.ClassName == "Folder" and #node.children == 0 then
		return nil
	end
	if inst.ClassName == "Folder" then
		materializedContainers[inst] = true
	end
	return node
end

-- Skeleton walker: records EVERY class's shape (class + name + children) with
-- no property values. The daemon keeps this as its live tree cache so external
-- tools (and the widget) know the full Studio tree, but the daemon never
-- attempts to round-trip non-ALLOWED classes.
local function skeletonNode(inst)
	local node = {
		class = inst.ClassName,
		name = inst.Name,
		children = {},
	}
	if hasAvoidSyncAttribute(inst) then
		node.avoidSync = true
		return node
	end
	for _, child in ipairs(inst:GetChildren()) do
		if not SUPPRESS_CLASSES[child.ClassName] then
			table.insert(node.children, skeletonNode(child))
		end
	end
	return node
end

--------------------------------------------------------------------------------
-- Path helpers
--------------------------------------------------------------------------------

local WATCHED_SET = {}
for _, name in ipairs(WATCHED_SERVICES) do
	WATCHED_SET[name] = true
end
local wallyRootSegments: { string }? = nil

local function isUnderWatched(inst)
	local cur = inst
	while cur and cur ~= game do
		local parent = cur.Parent
		if parent == game and WATCHED_SET[cur.Name] then
			return true
		end
		cur = parent
	end
	return false
end

local function pathFromRoot(inst)
	local segs = {}
	local cur = inst
	while cur and cur ~= game do
		table.insert(segs, 1, cur.Name)
		cur = cur.Parent
	end
	return segs
end

local function splitStudioPath(path: string): { string }
	local out = {}
	for seg in tostring(path or ""):gmatch("[^/]+") do
		table.insert(out, seg)
	end
	return out
end

local function parseDisambiguatedSegment(seg: string): (string?, number?)
	local base, ordinal = string.match(seg, "^(.-) %[([0-9]+)%]$")
	if type(base) ~= "string" or base == "" then
		return nil, nil
	end
	local n = tonumber(ordinal)
	if not n then
		return nil, nil
	end
	return base, n
end

local PathHelpers = {}

function PathHelpers.pathSegmentTaken(taken: { [string]: true }, candidate: string): boolean
	return taken[string.lower(candidate)] == true
end

function PathHelpers.generatedPathSegmentForChild(child: Instance, taken: { [string]: true }): string
	local base = child.Name
	if not PathHelpers.pathSegmentTaken(taken, base) then
		taken[string.lower(base)] = true
		return base
	end
	for n = 1, 9999 do
		local candidate = string.format("%s [%d]", base, n)
		if not PathHelpers.pathSegmentTaken(taken, candidate) then
			taken[string.lower(candidate)] = true
			return candidate
		end
	end
	local count = 0
	for _ in pairs(taken) do
		count += 1
	end
	local fallback = string.format("%s__%d", base, count)
	taken[string.lower(fallback)] = true
	return fallback
end

function PathHelpers.mappedSyncClass(inst: Instance): string
	if inst:IsA("LuaSourceContainer") or inst.ClassName == "Folder" then
		return inst.ClassName
	end
	return "Folder"
end

function PathHelpers.syncRelevantSignature(inst: Instance, mappedClass: string): string
	local parts = { mappedClass }
	local childKeys = {}
	for _, child in ipairs(inst:GetChildren()) do
		if not SUPPRESS_CLASSES[child.ClassName] and not isAvoidSyncBlocked(child) and isSyncable(child) then
			table.insert(childKeys, PathHelpers.syncPathSiblingSortKey(child))
		end
	end
	table.sort(childKeys)
	for _, key in ipairs(childKeys) do
		table.insert(parts, key)
	end
	return table.concat(parts, "\0")
end

function PathHelpers.syncPathSiblingSortKey(inst: Instance): string
	local mappedClass = PathHelpers.mappedSyncClass(inst)
	return table.concat({
		string.lower(inst.Name),
		inst.Name,
		mappedClass,
		PathHelpers.syncRelevantSignature(inst, mappedClass),
	}, "\0")
end

function PathHelpers.findGeneratedPathChild(parent: Instance, seg: string): Instance?
	local children = {}
	for _, child in ipairs(parent:GetChildren()) do
		if not SUPPRESS_CLASSES[child.ClassName] and not isAvoidSyncBlocked(child) and isSyncable(child) then
			table.insert(children, {
				key = PathHelpers.syncPathSiblingSortKey(child),
				child = child,
			})
		end
	end
	table.sort(children, function(a, b)
		return a.key < b.key
	end)
	local taken = {}
	for _, entry in ipairs(children) do
		local child = entry.child
		if PathHelpers.generatedPathSegmentForChild(child, taken) == seg then
			return child
		end
	end
	return nil
end

function PathHelpers.findRawDisambiguatedChild(parent: Instance, seg: string): Instance?
	local base, ordinal = parseDisambiguatedSegment(seg)
	if not base or not ordinal then
		return nil
	end
	local seen = 0
	for _, child in ipairs(parent:GetChildren()) do
		if child.Name == base then
			if seen == ordinal then
				return child
			end
			seen += 1
		end
	end
	return nil
end

function PathHelpers.findDisambiguatedChild(parent: Instance, seg: string): Instance?
	return PathHelpers.findGeneratedPathChild(parent, seg) or PathHelpers.findRawDisambiguatedChild(parent, seg)
end

local function pathStartsWith(path: { string }, prefix: { string }?): boolean
	if not prefix or #prefix == 0 or #path < #prefix then
		return false
	end
	for i, seg in ipairs(prefix) do
		if path[i] ~= seg then
			return false
		end
	end
	return true
end

local function isGeneratedDiskRoot(inst): boolean
	return pathStartsWith(pathFromRoot(inst), wallyRootSegments)
end

local function resolvePath(path)
	local cur = game
	for _, seg in ipairs(path) do
		local nxt
		if cur == game then
			local ok, svc = pcall(function()
				return game:GetService(seg)
			end)
			if ok and svc then
				nxt = svc
			else
				nxt = cur:FindFirstChild(seg)
			end
		else
			nxt = cur:FindFirstChild(seg)
		end
		if not nxt then
			nxt = PathHelpers.findDisambiguatedChild(cur, seg)
		end
		if not nxt then
			return nil
		end
		cur = nxt
	end
	return cur
end

local function parentPath(path)
	if type(path) ~= "table" or #path <= 1 then
		return nil
	end
	local parent = {}
	for i = 1, #path - 1 do
		parent[i] = path[i]
	end
	return parent
end

local function rootSetAllowed(op): boolean
	if type(op.path) == "table" and #op.path > 0 then
		return true
	end
	local node = op.node
	local name = type(node) == "table" and node.name or nil
	if type(name) == "string" and WATCHED_SET[name] == true then
		return true
	end
	log("SAFETY: ignoring root set for " .. tostring(name or "?"))
	return false
end

--------------------------------------------------------------------------------
-- Applying daemon ops (FS -> Studio)
--------------------------------------------------------------------------------

-- Clock-based echo guard (Argon-style). When `lastRemoteApplyClock` is within
-- ECHO_GUARD_WINDOW of os.clock(), Studio-side hooks suppress their op. Using a
-- timestamp instead of a mutable flag handles nested/re-entrant applies: any
-- Changed signals that fire during (or within ~100ms after) the apply are
-- correctly attributed to the remote apply rather than a user edit.
local lastRemoteApplyClock: number = -math.huge

local function isEchoing(): boolean
	return (os.clock() - lastRemoteApplyClock) < ECHO_GUARD_WINDOW
end

local function normalizeSourceForCompare(source: string): string
	if not source:find("\r", 1, true) then
		return source
	end
	return (source:gsub("\r\n", "\n"):gsub("\r", "\n"))
end

local function sourcesMatchForApply(currentSource: string, newSource: string): boolean
	if currentSource == newSource then
		return true
	end
	return normalizeSourceForCompare(currentSource) == normalizeSourceForCompare(newSource)
end

local function applyScriptSource(inst, newSource: string, suppressStudioEcho: boolean?): boolean
	-- Only filesystem-originated writes are echoes. Remote CLI writes are
	-- Studio-originated mutations and must be allowed through the normal
	-- Studio -> disk hooks. Treating them as daemon echoes loses the deferred
	-- ChildAdded/Changed signal entirely (most visibly for `rosync new` with an
	-- initial Source), leaving the new script live-only until reconnect.
	local suppressEcho = suppressStudioEcho ~= false
	if suppressEcho then
		lastRemoteApplyClock = os.clock()
	end
	if not ScriptEditorService then
		warn("[Ro Sync] ScriptEditorService unavailable; source write was not applied")
		return false
	end
	local ok, err = pcall(function()
		ScriptEditorService:UpdateSourceAsync(inst, function(_old)
			return newSource
		end)
	end)
	if suppressEcho then
		lastRemoteApplyClock = os.clock()
	end
	if not ok then
		warn("[Ro Sync] UpdateSourceAsync failed; source write was not applied: " .. tostring(err))
	end
	return ok
end

local function writeScriptSource(inst, newSource: string, suppressStudioEcho: boolean?): boolean
	if sourcesMatchForApply(readScriptSource(inst), newSource) then
		return true
	end
	return applyScriptSource(inst, newSource, suppressStudioEcho)
end

local function queueScriptSourceWrite(queue, inst, newSource: string): boolean
	if sourcesMatchForApply(readScriptSource(inst), newSource) then
		return false
	end
	table.insert(queue, {
		inst = inst,
		source = newSource,
	})
	return true
end

local function flushScriptSourceWrites(queue): number
	local total = #queue
	if total == 0 then
		return 0
	end

	local nextIndex = 0
	local finishedWorkers = 0
	local workers = math.min(SOURCE_WRITE_CONCURRENCY, total)
	local applied = 0

	for _ = 1, workers do
		task.spawn(function()
			while true do
				nextIndex += 1
				local job = queue[nextIndex]
				if not job then
					break
				end
				if applyScriptSource(job.inst, job.source) then
					applied += 1
				end
			end
			finishedWorkers += 1
		end)
	end

	while finishedWorkers < workers do
		lastRemoteApplyClock = os.clock()
		task.wait()
	end

	return applied
end

-- Source is the only property we round-trip. Scripts get rewritten via
-- ScriptEditorService so Studio's editor sees the new text; everything else
-- is silently dropped — non-script props are Studio-authoritative.
local function applyProperties(inst, props, opts)
	if type(props) ~= "table" then
		return
	end
	if props.Source ~= nil and inst:IsA("LuaSourceContainer") then
		local source = tostring(props.Source or "")
		if opts and opts.sourceWrites then
			queueScriptSourceWrite(opts.sourceWrites, inst, source)
		else
			if not writeScriptSource(inst, source) then
				error("source write was not applied for " .. inst:GetFullName())
			end
		end
	end
end

-- Count descendants cheaply; short-circuits at `cap` to avoid walking huge trees.
local function countDescendants(inst, cap: number): number
	local n = 0
	for _, child in ipairs(inst:GetChildren()) do
		n += 1
		if n >= cap then
			return n
		end
		n += countDescendants(child, cap - n)
		if n >= cap then
			return n
		end
	end
	return n
end

local function pathOf(inst: Instance): string
	if inst == game then
		return "game"
	end
	local parts = {}
	local cur = inst
	while cur and cur ~= game do
		table.insert(parts, 1, cur.Name)
		cur = cur.Parent
	end
	return table.concat(parts, "/")
end

-- Safety: refuse to Destroy a node that's either (a) a direct child of `game`
-- (i.e. a Service — Studio corruption if we Destroy these) or (b) carries a
-- subtree with any descendants. Class mismatches at depth 0 / depth 1 are the
-- exact failure mode seen when a bootstrap push was interrupted and disk later
-- re-emitted a service as class="Folder" — this turned into data loss.
local MAX_SAFE_DESTROY_DESCENDANTS = 0

local function canSafelyDestroy(inst: Instance, allowDescendants: boolean?): (boolean, string)
	if inst == game then
		return false, "root DataModel"
	end
	if inst.Parent == game then
		return false, "top-level service"
	end
	if allowDescendants == true then
		return true, ""
	end
	local n = countDescendants(inst, MAX_SAFE_DESTROY_DESCENDANTS + 1)
	if n > MAX_SAFE_DESTROY_DESCENDANTS then
		return false, string.format("%d+ descendants", n)
	end
	return true, ""
end

local function shouldStrictPruneChild(child: Instance, wanted: { [string]: boolean }): boolean
	if wanted[child.Name] then
		return false
	end
	if not ALLOWED_CLASSES[child.ClassName] then
		return false
	end
	if child:IsA("LuaSourceContainer") then
		return true
	end
	if child.ClassName == "Folder" then
		return hasSyncableDescendant(child)
	end
	return false
end

local function applyNode(parent, node, opts)
	if isAvoidSyncBlocked(parent) then
		log("AvoidSync: skipping disk apply under " .. pathOf(parent))
		return nil
	end
	-- Hard safety gate: only the four classes ro-sync is designed to
	-- round-trip ever get Instance.new()'d from disk-sourced payloads.
	-- Receiving anything else is a daemon/schema bug — log and bail before we
	-- can destroy an existing Studio instance to replace it with a stub.
	if not ALLOWED_CLASSES[node.class] then
		log(
			string.format(
				"SAFETY: skipping non-synced class %s at %s/%s",
				tostring(node.class),
				pathOf(parent),
				tostring(node.name)
			)
		)
		return nil
	end
	local existing = parent:FindFirstChild(node.name)
	if existing and isAvoidSyncBlocked(existing) then
		log("AvoidSync: skipping disk apply for " .. pathOf(existing))
		return existing
	end
	local inst
	if existing and existing.ClassName == node.class then
		inst = existing
	elseif existing and node.class == "Folder" and not ALLOWED_CLASSES[existing.ClassName] then
		-- Pass-through container: disk emits `class="Folder"` for any directory,
		-- but the Studio-side instance here is Studio-authoritative (a Model,
		-- Part, Tool, BoolValue, ...). Never replace it — treat it as the
		-- container for the incoming children. Its real class stays untouched.
		inst = existing
	else
		if existing then
			local safe, reason = canSafelyDestroy(existing, opts and opts.forcePrune == true)
			if not safe then
				log(
					string.format(
						"SAFETY: refusing to destroy %s (%s → %s) — %s. Keeping existing, skipping new.",
						pathOf(existing),
						existing.ClassName,
						tostring(node.class),
						reason
					)
				)
				-- Keep the existing instance and walk its children into the
				-- incoming node's children, so we don't nuke a subtree just
				-- because the top of it has a class mismatch (e.g. a service
				-- that disk briefly reported as a Folder).
				inst = existing
			else
				existing:Destroy()
			end
		end
		if not inst then
			local ok, created = pcall(function()
				return Instance.new(node.class)
			end)
			if not ok then
				log("unknown class: " .. tostring(node.class))
				return nil
			end
			inst = created
			inst.Name = node.name
			inst.Parent = parent
		end
	end
	applyProperties(inst, node.properties, opts)
	local wanted = {}
	for _, c in ipairs(node.children or {}) do
		wanted[c.name] = true
		applyNode(inst, c, opts)
	end
	if opts and opts.strict then
		for _, child in ipairs(inst:GetChildren()) do
			-- Strict pruning only removes script sources, plus Folder containers
			-- that still lead to script sources. Bare Studio folders are
			-- explorer-only and must not be destroyed just because disk lacks
			-- a matching directory.
			if shouldStrictPruneChild(child, wanted) then
				local safe, reason = canSafelyDestroy(child, opts and opts.forcePrune == true)
				if safe then
					child:Destroy()
				else
					log(string.format("SAFETY: strict-mode destroy skipped for %s — %s", pathOf(child), reason))
				end
			end
		end
	end
	return inst
end

local function applyClassChange(op)
	local newClass = tostring(op.class or "")
	if not SCRIPT_CLASSES[newClass] then
		log("class_change: unsupported class " .. newClass)
		return
	end
	local target = resolvePath(op.path)
	if not target or target == game then
		return
	end
	if isAvoidSyncBlocked(target) then
		log("AvoidSync: ignoring class_change for " .. pathOf(target))
		return
	end
	local toPath = type(op.to) == "table" and op.to or op.path
	local newName = toPath[#toPath] or target.Name
	local newParent = target.Parent
	local newParentPath = parentPath(toPath)
	if newParentPath then
		local resolvedParent = resolvePath(newParentPath)
		if resolvedParent then
			newParent = resolvedParent
		end
	end
	if not newParent or isAvoidSyncBlocked(newParent) then
		return
	end
	if target.ClassName == newClass then
		if target.Parent ~= newParent then
			target.Parent = newParent
		end
		if target.Name ~= newName then
			target.Name = newName
		end
		applyProperties(target, op.properties, nil)
		return
	end
	if not target:IsA("LuaSourceContainer") and target.ClassName ~= "Folder" then
		log(
			string.format("SAFETY: refusing class_change for %s (%s -> %s)", pathOf(target), target.ClassName, newClass)
		)
		return
	end
	local ok, replacement = pcall(function()
		return Instance.new(newClass)
	end)
	if not ok then
		log("class_change: unknown class " .. newClass)
		return
	end
	replacement.Name = newName
	replacement.Parent = newParent
	for _, child in ipairs(target:GetChildren()) do
		child.Parent = replacement
	end
	applyProperties(replacement, op.properties, nil)
	target:Destroy()
end

local function applyOp(op)
	local kind = op.op or op.type
	if kind == "set" or kind == "replace" then
		if not rootSetAllowed(op) then
			return
		end
		local parent
		if op.path and #op.path > 0 then
			parent = resolvePath(op.path)
		else
			parent = game
		end
		if not parent then
			log("set: parent path not found")
			return
		end
		if isAvoidSyncBlocked(parent) then
			log("AvoidSync: ignoring set under " .. pathOf(parent))
			return
		end
		applyNode(parent, op.node, { strict = op.strict == true })
	elseif kind == "delete" or kind == "remove" then
		local target = resolvePath(op.path)
		if target and target ~= game then
			if isAvoidSyncBlocked(target) then
				log("AvoidSync: ignoring delete for " .. pathOf(target))
				return
			end
			local safe, reason = canSafelyDestroy(target, op.forcePrune == true)
			if safe then
				target:Destroy()
			else
				log(
					string.format(
						"SAFETY: ignoring delete op for %s — %s. Rename or move it manually if this was intended.",
						pathOf(target),
						reason
					)
				)
			end
		end
	elseif kind == "update" then
		local target = resolvePath(op.path)
		if not target then
			return
		end
		if isAvoidSyncBlocked(target) then
			log("AvoidSync: ignoring update for " .. pathOf(target))
			return
		end
		-- Post-scope-down: update ops carry at most a Source string for a
		-- script. Name changes arrive as separate `rename` ops; every other
		-- property is Studio-authoritative and must not be applied here.
		local props = op.properties
		if type(props) == "table" and props.Source ~= nil and target:IsA("LuaSourceContainer") then
			if not writeScriptSource(target, tostring(props.Source or "")) then
				error("source update was not applied for " .. pathOf(target))
			end
		end
	elseif kind == "class_change" then
		applyClassChange(op)
	elseif kind == "rename" then
		if type(op.from) == "table" and type(op.to) == "table" then
			local target = resolvePath(op.from)
			if not target or target == game then
				return
			end
			if isAvoidSyncBlocked(target) then
				log("AvoidSync: ignoring rename for " .. pathOf(target))
				return
			end

			local toPath = op.to
			local newName = toPath[#toPath]
			local newParentPath = parentPath(toPath)
			if newParentPath then
				local newParent = resolvePath(newParentPath)
				if newParent and isAvoidSyncBlocked(newParent) then
					log("AvoidSync: ignoring rename into " .. pathOf(newParent))
					return
				end
				if newParent and target.Parent ~= newParent then
					target.Parent = newParent
				end
			end
			if newName and target.Name ~= newName then
				target.Name = newName
			end
		else
			local target = resolvePath(op.path)
			if target and op.name and not isAvoidSyncBlocked(target) then
				target.Name = op.name
			end
		end
	elseif kind == "move" then
		local target = resolvePath(op.from)
		local destination = type(op.to) == "table" and op.to or nil
		local destinationParent = destination and parentPath(destination) or nil
		local newParent = destinationParent and resolvePath(destinationParent) or nil
		if
			target
			and destination
			and newParent
			and not isAvoidSyncBlocked(target)
			and not isAvoidSyncBlocked(newParent)
		then
			target.Parent = newParent
			local newName = destination[#destination]
			if newName and target.Name ~= newName then
				target.Name = newName
			end
		end
	else
		log("unknown op: " .. tostring(kind))
	end
end

local function setWaypoint(label: string)
	pcall(function()
		ChangeHistoryService:SetWaypoint(label)
	end)
end

local function summarizeOps(ops): string
	if not ops or #ops == 0 then
		return "noop"
	end
	local counts: { [string]: number } = {}
	for _, op in ipairs(ops) do
		local k = tostring(op.op or op.type or "?")
		counts[k] = (counts[k] or 0) + 1
	end
	local parts = {}
	for k, n in pairs(counts) do
		table.insert(parts, string.format("%s×%d", k, n))
	end
	return table.concat(parts, ",")
end

local function applyOps(ops)
	if not ops or #ops == 0 then
		return
	end
	setWaypoint("Ro Sync: before " .. summarizeOps(ops))
	lastRemoteApplyClock = os.clock()
	local ok, err = pcall(function()
		for _, op in ipairs(ops) do
			applyOp(op)
		end
	end)
	lastRemoteApplyClock = os.clock()
	if not ok then
		log("applyOps error: " .. tostring(err))
	end
	setWaypoint("Ro Sync: " .. summarizeOps(ops))
end

local function sourceAckForAppliedOp(op)
	local kind = op.op or op.type
	local expected
	local path
	if kind == "set" or kind == "replace" then
		local node = op.node
		if type(node) ~= "table" or type(node.properties) ~= "table" or node.properties.Source == nil then
			return nil
		end
		expected = tostring(node.properties.Source or "")
		path = {}
		for _, segment in ipairs(op.path or {}) do
			table.insert(path, segment)
		end
		table.insert(path, tostring(node.name or ""))
	elseif kind == "update" then
		if type(op.properties) ~= "table" or op.properties.Source == nil then
			return nil
		end
		expected = tostring(op.properties.Source or "")
		path = op.path
	elseif kind == "class_change" then
		if type(op.properties) ~= "table" or op.properties.Source == nil then
			return nil
		end
		expected = tostring(op.properties.Source or "")
		path = type(op.to) == "table" and op.to or op.path
	else
		return nil
	end

	if type(path) ~= "table" then
		return nil
	end
	local target = resolvePath(path)
	if not target or not target:IsA("LuaSourceContainer") then
		return nil
	end
	local actual = readScriptSource(target)
	if not sourcesMatchForApply(actual, expected) then
		return nil
	end
	return {
		op = "update",
		path = path,
		properties = { Source = actual },
	}
end

local function applySnapshot(snap)
	-- Apply a full snapshot payload returned by /snapshot.
	setWaypoint("Ro Sync: before snapshot")
	lastRemoteApplyClock = os.clock()
	local ok, err = pcall(function()
		if snap and snap.services then
			local strict = snap.strict == true
			local forcePrune = snap.forcePrune == true
			local sourceWrites = {}
			local applyOpts = {
				strict = strict,
				forcePrune = forcePrune,
				sourceWrites = sourceWrites,
			}
			for _, node in ipairs(snap.services) do
				local svc = game:GetService(node.name)
				if svc then
					-- ALLOWED whitelist rejects non-ALLOWED (top-level services
					-- aren't ALLOWED). Services already exist, so apply each
					-- child directly into the resolved service.
					local wanted = {}
					for _, child in ipairs(node.children or {}) do
						wanted[child.name] = true
						applyNode(svc, child, applyOpts)
					end
					if strict then
						for _, existing in ipairs(svc:GetChildren()) do
							if shouldStrictPruneChild(existing, wanted) then
								local safe, reason = canSafelyDestroy(existing, forcePrune)
								if safe then
									existing:Destroy()
								else
									log(
										string.format(
											"SAFETY: strict-mode destroy skipped for %s — %s",
											pathOf(existing),
											reason
										)
									)
								end
							end
						end
					end
				end
			end
			if #sourceWrites > 0 then
				local appliedSources = flushScriptSourceWrites(sourceWrites)
				log(
					string.format(
						"snapshot source writes: %d/%d applied with %d workers",
						appliedSources,
						#sourceWrites,
						math.min(SOURCE_WRITE_CONCURRENCY, #sourceWrites)
					)
				)
			end
		elseif snap and snap.ops then
			for _, op in ipairs(snap.ops) do
				applyOp(op)
			end
		end
	end)
	lastRemoteApplyClock = os.clock()
	if not ok then
		log("applySnapshot error: " .. tostring(err))
	end
	setWaypoint("Ro Sync: snapshot applied")
end

--------------------------------------------------------------------------------
-- HTTP helpers
--------------------------------------------------------------------------------

local baseUrl = ""

-- Connection state + active WebSocket client, hoisted so earlier-declared
-- closures (flushPush, disconnect) can reach them — Lua resolves name
-- references lexically at function-definition time, so these must exist
-- before flushPush is parsed.
local connected = false
local busy = false -- probing / connecting / waiting-for-decision
local wsGeneration = 0
local decisionGeneration = 0
local connectedPort: string? = nil
local ws: any = nil
-- Desired connection state and recovery helpers share one table to keep this
-- already-large top-level chunk comfortably below Luau's 200-register limit.
-- `desired` stays true across transport/daemon restarts; only an explicit user
-- cancel, plugin unload, or non-retryable protocol rejection turns it off.
local reconnectState = {
	desired = false,
	compareRetryPending = false,
	discoverDaemon = nil,
	-- Short-lived proof from /hello. Never persist or log it; every fresh
	-- daemon process issues a new capability.
	pluginCapability = nil,
	-- Capability against which the most recent full initial comparison ran.
	-- A different value at WebSocket time means the daemon changed between
	-- snapshot reconciliation and live transport authentication.
	comparedCapability = nil,
}
-- Forward-declared so wsLoop (below) can re-enter the initial-sync handshake
-- after any established transport gap. The real function assignment happens
-- further down.
local runInitialCompare: () -> ()

-- Strip NULs and replace invalid UTF-8 bytes with the Unicode replacement
-- char (EF BF BD). JSONEncode rejects either. We keep valid UTF-8 intact.
local function scrubString(s)
	if #s == 0 then
		return s
	end
	-- Fast path: no NUL and pure ASCII (< 0x80).
	if not s:find("[%z\128-\255]") then
		return s
	end
	local out = {}
	local i = 1
	local len = #s
	while i <= len do
		local b = s:byte(i)
		if b == 0 then
			i += 1
		elseif b < 0x80 then
			out[#out + 1] = string.char(b)
			i += 1
		else
			local seqLen
			if b >= 0xF0 then
				seqLen = 4
			elseif b >= 0xE0 then
				seqLen = 3
			elseif b >= 0xC0 then
				seqLen = 2
			else
				seqLen = 0
			end
			if seqLen > 0 and i + seqLen - 1 <= len then
				local ok = true
				for j = 1, seqLen - 1 do
					local cb = s:byte(i + j)
					if cb < 0x80 or cb >= 0xC0 then
						ok = false
						break
					end
				end
				if ok then
					out[#out + 1] = s:sub(i, i + seqLen - 1)
					i += seqLen
				else
					out[#out + 1] = "\239\191\189"
					i += 1
				end
			else
				out[#out + 1] = "\239\191\189"
				i += 1
			end
		end
	end
	return table.concat(out)
end

-- Recursively sanitize any value tree so HttpService:JSONEncode never sees a
-- non-JSON type (userdata, Instance, CFrame, NaN, etc.). Last line of defense
-- in case serializeProperties misses something.
local function sanitizeJson(v, depth)
	depth = depth or 0
	if depth > 128 then
		return nil
	end
	local t = typeof(v)
	if t == "nil" or t == "boolean" then
		return v
	elseif t == "string" then
		return scrubString(v)
	elseif t == "number" then
		if v ~= v or v == math.huge or v == -math.huge then
			return nil
		end
		return v
	elseif t == "table" then
		local isArray = true
		for k in pairs(v) do
			if type(k) ~= "number" then
				isArray = false
				break
			end
		end
		if isArray then
			local out = {}
			for i, item in ipairs(v) do
				local cleaned = sanitizeJson(item, depth + 1)
				if cleaned ~= nil then
					out[#out + 1] = cleaned
				end
			end
			return out
		else
			local out = {}
			for k, item in pairs(v) do
				if type(k) == "string" then
					local cleaned = sanitizeJson(item, depth + 1)
					if cleaned ~= nil then
						out[scrubString(k)] = cleaned
					end
				end
			end
			return out
		end
	end
	return nil
end

local function httpRequestTo(url, method, body)
	local req = {
		Url = url,
		Method = method or "GET",
		Headers = { ["Content-Type"] = "application/json" },
	}
	if body ~= nil then
		local clean = sanitizeJson(body)
		local encOk, enc = pcall(function()
			return HttpService:JSONEncode(clean)
		end)
		if not encOk then
			return false, "JSONEncode failed: " .. tostring(enc)
		end
		req.Body = enc
	end
	local ok, resp = pcall(function()
		return HttpService:RequestAsync(req)
	end)
	if not ok then
		return false, tostring(resp)
	end
	return true, resp
end

local function httpRequest(path, method, body)
	return httpRequestTo(baseUrl .. path, method, body)
end

local function httpJson(path, method, body)
	local ok, resp = httpRequest(path, method, body)
	if not ok then
		return false, resp
	end
	if not resp.Success then
		return false, string.format("HTTP %d %s", resp.StatusCode, resp.StatusMessage or "")
	end
	if resp.Body == nil or resp.Body == "" then
		return true, nil
	end
	local decoded
	local decOk, err = pcall(function()
		decoded = HttpService:JSONDecode(resp.Body)
	end)
	if not decOk then
		return false, "decode: " .. tostring(err)
	end
	return true, decoded
end

--------------------------------------------------------------------------------
-- Pushing local changes (Studio -> FS)
--------------------------------------------------------------------------------

local pendingOps = {}
local pushPending = false
local pendingEditorSourceByScript: { [Instance]: { source: string, serial: number } } = {}
local EDITOR_SOURCE_DEBOUNCE = 0.35
local markTreeDirty
local flushPush

local function prependPendingOps(ops)
	for i = #ops, 1, -1 do
		table.insert(pendingOps, 1, ops[i])
	end
end

local function schedulePushRetry(delaySeconds: number?)
	if pushPending then
		return
	end
	pushPending = true
	task.delay(delaySeconds or 0.25, function()
		pushPending = false
		flushPush()
	end)
end

flushPush = function()
	if #pendingOps == 0 then
		return
	end
	local client = ws
	if not client then
		-- No active WS. If the plugin is still nominally connected (client is
		-- mid-reconnect inside wsLoop), defer one debounce tick and retry; the
		-- reconnect backoff caps at 30s so this can stall briefly but won't
		-- spin. If fully disconnected, drop.
		if not connected then
			pendingOps = {}
			return
		end
		schedulePushRetry(0.25)
		return
	end
	local ops = pendingOps
	pendingOps = {}
	local msg = { type = "push", ops = ops }
	local clean = sanitizeJson(msg)
	local encOk, enc = pcall(function()
		return HttpService:JSONEncode(clean)
	end)
	if not encOk then
		log("push encode fail: " .. tostring(enc))
		return
	end
	local sendOk, sendErr = pcall(function()
		client:Send(enc)
	end)
	if not sendOk then
		prependPendingOps(ops)
		log(string.format("push err: %s (%d op(s) requeued)", tostring(sendErr), #ops))
		if ws == client then
			ws = nil
		end
		pcall(function()
			client:Close()
		end)
		schedulePushRetry(0.5)
	end
end

local function queueOp(op)
	if isEchoing() then
		return
	end
	table.insert(pendingOps, op)
	if not pushPending then
		pushPending = true
		task.delay(0.1, function()
			pushPending = false
			flushPush()
		end)
	end
end

local function queueScriptUpdateOp(inst: Instance, source: string)
	markTreeDirty()
	queueOp({
		op = "update",
		path = pathFromRoot(inst),
		properties = { Source = source },
	})
end

local function scriptFromDocument(document): Instance?
	local okCommandBar, isCommandBar = pcall(function()
		return document:IsCommandBar()
	end)
	if okCommandBar and isCommandBar then
		return nil
	end
	local ok, scriptInst = pcall(function()
		return document:GetScript()
	end)
	if ok and typeof(scriptInst) == "Instance" and scriptInst:IsA("LuaSourceContainer") then
		return scriptInst
	end
	return nil
end

local function sourceFromDocument(document, scriptInst: Instance): string?
	local okText, text = pcall(function()
		return document:GetText()
	end)
	if okText and type(text) == "string" then
		return text
	end
	return readScriptSource(scriptInst)
end

local function queueEditorSourceChanged(scriptInst: Instance, source: string)
	local pending = pendingEditorSourceByScript[scriptInst]
	if pending and pending.source == source then
		return
	end
	local serial = pending and (pending.serial + 1) or 1
	pendingEditorSourceByScript[scriptInst] = {
		source = source,
		serial = serial,
	}
	task.delay(EDITOR_SOURCE_DEBOUNCE, function()
		local latest = pendingEditorSourceByScript[scriptInst]
		if not latest or latest.serial ~= serial then
			return
		end
		pendingEditorSourceByScript[scriptInst] = nil
		if isEchoing() then
			return
		end
		if scriptInst.Parent == nil then
			return
		end
		if not isUnderWatched(scriptInst) then
			return
		end
		if isGeneratedDiskRoot(scriptInst) then
			return
		end
		if isAvoidSyncBlocked(scriptInst) then
			return
		end
		if not scriptInst:IsA("LuaSourceContainer") then
			return
		end
		queueScriptUpdateOp(scriptInst, latest.source)
	end)
end

--------------------------------------------------------------------------------
-- Tree skeleton push (Studio -> daemon live tree cache)
--------------------------------------------------------------------------------
-- Payload can be hundreds of KB on a moderately-sized place (every Instance
-- under every watched service), so we use HTTP POST instead of piling it into
-- the WebSocket (which has smaller frame limits and shares a backpressure
-- budget with per-op traffic).

local TREE_PUSH_DEBOUNCE = 0.5
local treeDirty = false
local treePushPending = false

local function collectTreeSkeleton()
	local services = {}
	for _, name in ipairs(WATCHED_SERVICES) do
		local ok, svc = pcall(function()
			return game:GetService(name)
		end)
		if ok and svc then
			table.insert(services, skeletonNode(svc))
		end
	end
	return services
end

local function pushTreeSkeleton()
	if not treeDirty then
		return
	end
	if not connected then
		return
	end
	local ok, err = httpJson("/tree", "POST", collectTreeSkeleton())
	if ok then
		treeDirty = false
	else
		-- Leave treeDirty set so the next mark/tick retries; the daemon is
		-- likely restarting or the socket is mid-flap. Don't spam the log —
		-- reconnect path will log its own failure context.
		log("tree push err: " .. tostring(err))
	end
end

markTreeDirty = function()
	treeDirty = true
	if treePushPending then
		return
	end
	treePushPending = true
	task.delay(TREE_PUSH_DEBOUNCE, function()
		treePushPending = false
		pushTreeSkeleton()
	end)
end

--------------------------------------------------------------------------------
-- Hooks
--------------------------------------------------------------------------------

-- Per-instance connection table (Argon Watcher.luau style). Each instance
-- carries its own Changed / ChildAdded / ChildRemoved connections. When an
-- instance is removed we walk its subtree and disconnect everything — this
-- prevents connection leaks under rapid add/remove event storms and keeps
-- signal fan-out proportional to live descendants rather than historical ones.
local connections: { [Instance]: { RBXScriptConnection } } = {}
local serviceConnections: { RBXScriptConnection } = {}
-- Name at the time we first connected the instance. When Changed fires for
-- "Name", the instance already holds the new name and pathFromRoot would
-- reconstruct the new path — useless for telling the daemon *which* file to
-- rename. Tracking the prior name here lets us emit a `rename` op with the
-- correct old path + new name.
local lastKnownName: { [Instance]: string } = {}
-- Full path captured while the instance is still parented. Roblox fires
-- ChildRemoved after Parent becomes nil, so recomputing the path in that
-- callback loses the service/ancestor chain and silently drops delete ops.
local lastKnownPath: { [Instance]: { string } } = {}
-- `materializedContainers` is declared near the top of the file alongside
-- `serializeInstance`, which writes to it as a side effect during bootstrap.

-- Forward declarations so the event callbacks can refer to each other. These
-- are plain locals rather than `local function` so the later `function foo()`
-- sugar unambiguously rebinds the existing local in all Lua/Luau variants.
local onDescendantAdded
local onDescendantRemoving
local onChanged
local onAvoidSyncChanged
local connectInstance
local connectTree
local disconnectInstance
local disconnectTree

local function refreshKnownPaths(inst)
	lastKnownPath[inst] = pathFromRoot(inst)
	for _, child in ipairs(inst:GetChildren()) do
		refreshKnownPaths(child)
	end
end

-- Walk `inst` and its descendants; record every directory-backed container,
-- since emitting a `set` implicitly materializes those paths on disk.
local function recordMaterializedContainers(inst)
	if SUPPRESS_CLASSES[inst.ClassName] then
		return
	end
	if isAvoidSyncBlocked(inst) then
		return
	end
	if inst.ClassName == "Folder" or not ALLOWED_CLASSES[inst.ClassName] then
		materializedContainers[inst] = true
	end
	for _, child in ipairs(inst:GetChildren()) do
		recordMaterializedContainers(child)
	end
end

-- Hooks are installed only after the initial disk/Studio decision. A disk-win
-- bootstrap can therefore create a whole Folder/script tree before ChildAdded
-- is being observed. Seed the materialization cache from that existing tree so
-- deleting its top-level Folder later still removes the disk subtree. This is
-- a bottom-up walk instead of calling isSyncable for every live instance; large
-- places commonly contain many Studio-authoritative descendants, and the
-- bottom-up form keeps hook installation linear in the size of the DataModel.
local function seedExistingMaterializedContainers(inst): boolean
	if SUPPRESS_CLASSES[inst.ClassName] or isAvoidSyncBlocked(inst) then
		return false
	end

	local hasScript = inst:IsA("LuaSourceContainer")
	for _, child in ipairs(inst:GetChildren()) do
		if seedExistingMaterializedContainers(child) then
			hasScript = true
		end
	end

	if hasScript and (inst.ClassName == "Folder" or not ALLOWED_CLASSES[inst.ClassName]) then
		materializedContainers[inst] = true
	end
	return hasScript
end

local function recordMaterializedAncestors(inst)
	local current = inst
	while current and current.Parent and current.Parent ~= game do
		if SUPPRESS_CLASSES[current.ClassName] or isAvoidSyncBlocked(current) then
			return
		end
		if current.ClassName == "Folder" or not ALLOWED_CLASSES[current.ClassName] then
			materializedContainers[current] = true
		end
		current = current.Parent
	end
end

onDescendantAdded = function(inst)
	if isEchoing() then
		return
	end
	if not isUnderWatched(inst) then
		return
	end
	if isAvoidSyncBlocked(inst) then
		markTreeDirty()
		return
	end
	if isGeneratedDiskRoot(inst) then
		markTreeDirty()
		return
	end
	if SUPPRESS_CLASSES[inst.ClassName] then
		return
	end
	-- Any Studio-side addition — synced OR Studio-authoritative — changes the
	-- skeleton. Mark dirty so pushTreeSkeleton picks it up on the next tick.
	markTreeDirty()
	-- Emit a `set` for:
	--   (a) ALLOWED classes directly, or
	--   (b) non-ALLOWED classes that already have ALLOWED descendants (e.g. a
	--       Part being reparented into Workspace with a Script already inside
	--       it — we need the pass-through directory to materialize at once).
	-- A bare non-ALLOWED with no syncable descendants is live-tree-only.
	if not isSyncable(inst) then
		return
	end
	-- When a script is added directly inside an existing pass-through Part,
	-- this fires with `inst = Script` whose parent is the Part. The Part's
	-- directory on disk is created implicitly when the daemon writes the
	-- script file — but we still mark the Part as materialized so a later
	-- Removing on the Part emits a cleanup delete.
	recordMaterializedAncestors(inst.Parent)
	recordMaterializedContainers(inst)
	queueOp({
		op = "set",
		path = pathFromRoot(inst.Parent),
		node = serializeInstance(inst),
	})
end

onDescendantRemoving = function(inst)
	if isEchoing() then
		return
	end
	local path = lastKnownPath[inst]
	if not path or not WATCHED_SET[path[1]] then
		return
	end
	if isAvoidSyncBlocked(inst) then
		markTreeDirty()
		return
	end
	markTreeDirty()
	if isGeneratedDiskRoot(inst) then
		return
	end
	-- `Removing` fires top-down as children are re-parented to nil: by the
	-- time it's the pass-through container's turn, its syncable children have
	-- already been detached and `hasSyncableDescendant` returns false. Use
	-- the `materializedContainers` record to recover: if we ever emitted this
	-- instance as a directory on disk, emit a cleanup delete regardless.
	local syncable = isSyncable(inst) or materializedContainers[inst] == true
	materializedContainers[inst] = nil
	if not syncable then
		return
	end
	log(string.format("delete: %s", table.concat(path, "/")))
	queueOp({
		op = "delete",
		path = path,
	})
end

onChanged = function(inst, prop)
	if isEchoing() then
		return
	end
	if not isUnderWatched(inst) then
		return
	end
	if isGeneratedDiskRoot(inst) then
		return
	end
	if isAvoidSyncBlocked(inst) then
		if prop == "Name" then
			markTreeDirty()
		end
		return
	end
	-- Parent changes are observed via ChildAdded/ChildRemoved on the
	-- surrounding instances; emitting here would double-report moves.
	if prop == "Parent" then
		return
	end
	if prop == "Name" then
		-- Renames affect every live-tree consumer, not just the synced files.
		markTreeDirty()
		-- Non-syncable instances don't exist on disk, so there's no file to
		-- rename — the live-tree push above is all the daemon needs.
		if not isSyncable(inst) then
			lastKnownName[inst] = inst.Name
			refreshKnownPaths(inst)
			return
		end
		-- Dedicated rename op: daemon needs the OLD path to locate the file
		-- it has to rename. pathFromRoot(inst) already reflects the new name
		-- because Changed fires after the assignment, so we reconstruct the
		-- old path from the cached last-known name.
		local oldName = lastKnownName[inst] or inst.Name
		local oldPath = lastKnownPath[inst]
		lastKnownName[inst] = inst.Name
		if not oldPath then
			local parentPath = inst.Parent and pathFromRoot(inst.Parent) or {}
			oldPath = {}
			for i, s in ipairs(parentPath) do
				oldPath[i] = s
			end
			table.insert(oldPath, oldName)
		end
		refreshKnownPaths(inst)
		log(string.format("rename: %s → %s (path depth %d)", oldName, inst.Name, #oldPath))
		queueOp({
			op = "rename",
			path = oldPath,
			name = inst.Name,
		})
		return
	end
	if prop == "Source" and inst:IsA("LuaSourceContainer") then
		queueScriptUpdateOp(inst, readScriptSource(inst))
		return
	end
	-- Every other property is Studio-authoritative. No op, no tree push —
	-- live-tree shape depends on class+name+children, none of which change
	-- on a plain property edit.
end

onAvoidSyncChanged = function(inst)
	if isEchoing() then
		return
	end
	if not isUnderWatched(inst) then
		return
	end
	markTreeDirty()
	if hasAvoidSyncAttribute(inst) then
		log("AvoidSync: ignoring " .. pathOf(inst))
		return
	end
	connectTree(inst)
	if isSyncable(inst) and inst.Parent then
		queueOp({
			op = "set",
			path = pathFromRoot(inst.Parent),
			node = serializeInstance(inst),
		})
	end
end

disconnectInstance = function(inst)
	local bucket = connections[inst]
	if not bucket then
		return
	end
	for _, c in ipairs(bucket) do
		pcall(function()
			c:Disconnect()
		end)
	end
	connections[inst] = nil
	lastKnownName[inst] = nil
	lastKnownPath[inst] = nil
	materializedContainers[inst] = nil
end

disconnectTree = function(inst)
	for _, child in ipairs(inst:GetChildren()) do
		disconnectTree(child)
	end
	disconnectInstance(inst)
end

connectInstance = function(inst)
	if connections[inst] then
		return
	end
	if SUPPRESS_CLASSES[inst.ClassName] then
		return
	end
	local bucket: { RBXScriptConnection } = {}
	connections[inst] = bucket
	lastKnownName[inst] = inst.Name
	lastKnownPath[inst] = pathFromRoot(inst)
	table.insert(
		bucket,
		inst.Changed:Connect(function(prop)
			onChanged(inst, prop)
		end)
	)
	table.insert(
		bucket,
		inst:GetAttributeChangedSignal(AVOID_SYNC_ATTRIBUTE):Connect(function()
			onAvoidSyncChanged(inst)
		end)
	)
	table.insert(
		bucket,
		inst.ChildAdded:Connect(function(child)
			onDescendantAdded(child)
			connectTree(child)
		end)
	)
	table.insert(
		bucket,
		inst.ChildRemoved:Connect(function(child)
			onDescendantRemoving(child)
			disconnectTree(child)
		end)
	)
end

connectTree = function(inst)
	if isGeneratedDiskRoot(inst) then
		return
	end
	connectInstance(inst)
	if isAvoidSyncBlocked(inst) then
		return
	end
	for _, child in ipairs(inst:GetChildren()) do
		connectTree(child)
	end
end

local function disconnectHooks()
	for _, c in ipairs(serviceConnections) do
		pcall(function()
			c:Disconnect()
		end)
	end
	serviceConnections = {}
	pendingEditorSourceByScript = {}
	for inst, _ in pairs(connections) do
		disconnectInstance(inst)
	end
	connections = {}
end

local function hookService(svc)
	-- The service itself is a root — connect its descendants individually so
	-- each can be torn down on removal. We skip the service's own Changed
	-- signal (its properties aren't part of the sync surface).
	-- Seed first: a disk-win initial bootstrap may have created this tree before
	-- hooks existed, so there were no ChildAdded callbacks to populate the
	-- materialized-container delete cache.
	seedExistingMaterializedContainers(svc)
	table.insert(
		serviceConnections,
		svc.ChildAdded:Connect(function(child)
			if isGeneratedDiskRoot(child) then
				markTreeDirty()
				return
			end
			onDescendantAdded(child)
			connectTree(child)
		end)
	)
	table.insert(
		serviceConnections,
		svc.ChildRemoved:Connect(function(child)
			if isGeneratedDiskRoot(child) then
				markTreeDirty()
				return
			end
			onDescendantRemoving(child)
			disconnectTree(child)
		end)
	)
	for _, child in ipairs(svc:GetChildren()) do
		connectTree(child)
	end
end

local function installHooks()
	disconnectHooks()
	for _, name in ipairs(WATCHED_SERVICES) do
		local ok, svc = pcall(function()
			return game:GetService(name)
		end)
		if ok and svc then
			hookService(svc)
		else
			log("skip service: " .. name)
		end
	end
	if ScriptEditorService then
		table.insert(
			serviceConnections,
			ScriptEditorService.TextDocumentDidChange:Connect(function(document, _changes)
				if isEchoing() then
					return
				end
				local scriptInst = scriptFromDocument(document)
				if not scriptInst then
					return
				end
				if not isUnderWatched(scriptInst) then
					return
				end
				if isGeneratedDiskRoot(scriptInst) then
					return
				end
				if isAvoidSyncBlocked(scriptInst) then
					return
				end
				local source = sourceFromDocument(document, scriptInst)
				if type(source) ~= "string" then
					return
				end
				queueEditorSourceChanged(scriptInst, source)
			end)
		)
	end
end

--------------------------------------------------------------------------------
-- Studio stats (for initial-compare)
--------------------------------------------------------------------------------

-- Initial-compare stats are now only a legacy/empty-side signal; the daemon
-- uses the Studio snapshot for real local-vs-Studio differences. We count what
-- will actually be materialized on disk:
-- ALLOWED classes plus pass-through containers (non-ALLOWED instances that
-- have an ALLOWED descendant). Parts / Models with no scripts underneath are
-- Studio-authoritative (live-tree only) and don't count.
local function countSyncableSubtree(inst)
	if SUPPRESS_CLASSES[inst.ClassName] then
		return 0, 0, false
	end
	local isScript = inst:IsA("LuaSourceContainer")
	local kidsInst, kidsScript, kidsHasScript = 0, 0, false
	for _, child in ipairs(inst:GetChildren()) do
		local ci, cs, ch = countSyncableSubtree(child)
		kidsInst += ci
		kidsScript += cs
		if ch then
			kidsHasScript = true
		end
	end
	local subtreeHasScript = isScript or kidsHasScript
	local selfCountable = ALLOWED_CLASSES[inst.ClassName] == true or subtreeHasScript
	if selfCountable then
		return kidsInst + 1, kidsScript + (isScript and 1 or 0), subtreeHasScript
	end
	return kidsInst, kidsScript, subtreeHasScript
end

local function collectStudioStats()
	local scriptCount = 0
	local instanceCount = 0
	for _, name in ipairs(WATCHED_SERVICES) do
		local ok, svc = pcall(function()
			return game:GetService(name)
		end)
		if ok and svc then
			for _, child in ipairs(svc:GetChildren()) do
				local ci, cs = countSyncableSubtree(child)
				instanceCount += ci
				scriptCount += cs
			end
		end
	end
	return { scriptCount = scriptCount, instanceCount = instanceCount }
end

-- Snapshot for /push bootstrap. serializeInstance emits ALLOWED classes
-- directly and non-ALLOWED classes as pass-through Folders when they lead to
-- at least one script. A service whose descendants are entirely non-syncable
-- still emits a node with an empty children[] — the daemon treats that as a
-- no-op and doesn't materialize the service directory on disk.
local function collectSnapshot()
	local services = {}
	for _, name in ipairs(WATCHED_SERVICES) do
		local ok, svc = pcall(function()
			return game:GetService(name)
		end)
		if ok and svc then
			local node = serializeSnapshotInstance(svc)
			if node then
				table.insert(services, node)
			end
		end
	end
	return services
end

--------------------------------------------------------------------------------
-- Connection state
--------------------------------------------------------------------------------
-- The actual `connected` / `busy` / `wsGeneration` / `decisionGeneration` /
-- `connectedPort` / `ws` locals are hoisted to the Transport state block
-- higher up so flushPush can reach them (Lua binds upvalues lexically at
-- function-definition time).

--------------------------------------------------------------------------------
-- WebSocket transport
--------------------------------------------------------------------------------
-- Plugin ↔ daemon realtime path. Replaces the prior long-poll (/poll) and
-- (widget-side) SSE (/events). All non-realtime routes stay on HTTP:
--   /hello, /snapshot, /push (bootstrap), /initial-compare, /initial-decision,
--   /resolve, /config.
--
-- Frame format: JSON object over text WebSocket messages. Daemon uses serde
-- tagged enums with tag="type". See team-chat agreement (Agent 1):
--
--   Client → Server:
--     {type="hello", clientId=<gameId>}
--     {type="push",  ops={...}}            -- same op shape as HTTP /push
--     {type="ping"} | {type="pong"}
--
--   Server → Client:
--     {type="op",    op={...}}             -- NESTED: the inner op is the
--                                          -- same shape applyOp consumes
--     {type="lagged"}                      -- broadcast overflow, plugin
--                                          -- must reconnect + resync
--     {type="ping"} | {type="pong"}
--     {type="push-result", ok=..., applied=..., skipped=..., conflicts=..., errors=...}
--     {type="error", error=...}            -- daemon couldn't parse our frame
--
--   Events are forwarded from the daemon's state.events broadcast with their
--   ORIGINAL top-level type intact (daemon does NOT rewrap them in an "event"
--   variant). Expected values: "conflict", "config-changed",
--   "initial-choice-needed", "initial-choice-made". The plugin only logs
--   them; the initial-sync handshake still drives decisions via the HTTP
--   /initial-decision polling path.
--
-- Reconnect: exponential backoff 1s → 30s cap, reset after a successful hello.
-- Heartbeat: plugin sends {type="ping"} every WS_HEARTBEAT_INTERVAL seconds;
-- daemon replies with pong and also pings us every 10s. If no server traffic
-- arrives within WS_HEARTBEAT_TIMEOUT, plugin closes to force a reconnect.
-- Lag: on {type="lagged"} plugin closes the socket, tears down hooks, and
-- re-runs /initial-compare to resync state (ops may have been dropped).

local WS_BACKOFF_START = 1
local WS_BACKOFF_MAX = 30
local WS_HEARTBEAT_INTERVAL = 20
local WS_HEARTBEAT_TIMEOUT = 45

local wsBackoff = WS_BACKOFF_START

local function toWsUrl(httpUrl: string): string
	local u = tostring(httpUrl or "")
	u = u:gsub("^https://", "wss://")
	u = u:gsub("^http://", "ws://")
	return u .. "/ws"
end

local function wsSend(client, msg): boolean
	if not client then
		return false
	end
	local clean = sanitizeJson(msg)
	local encOk, enc = pcall(function()
		return HttpService:JSONEncode(clean)
	end)
	if not encOk then
		log("ws send encode fail: " .. tostring(enc))
		return false
	end
	local ok, err = pcall(function()
		client:Send(enc)
	end)
	if not ok then
		log("ws send err: " .. tostring(err))
		return false
	end
	return true
end

local function parseWsPayload(payload): any
	local text
	local t = typeof(payload)
	if t == "string" then
		text = payload
	elseif t == "buffer" then
		local ok, s = pcall(function()
			return buffer.tostring(payload)
		end)
		if ok then
			text = s
		end
	else
		return nil
	end
	if not text or text == "" then
		return nil
	end
	local decoded
	local decOk, err = pcall(function()
		decoded = HttpService:JSONDecode(text)
	end)
	if not decOk then
		log("ws decode err: " .. tostring(err))
		return nil
	end
	if type(decoded) ~= "table" then
		return nil
	end
	return decoded
end

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
	local moduleScript = script:FindFirstChild("Clipboard")
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
		local moduleScript = script:FindFirstChild("Playscript")
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
				for _, connection in ipairs(service:GetPluginConnectionsOfType(Enum.PluginConnectionTargetType.Test)) do
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
					tostring(runtimeError.code or "RUNTIME_ERROR") .. ": " .. tostring(runtimeError.message or "failed")
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
			if not canLeave or allowed ~= true then
				return false, "Studio cannot leave this playtest"
			end
			local ok, result = pcall(function()
				return service:LeaveTest()
			end)
			if not ok then
				return false, tostring(result)
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

		if forceStop then
			local left, leaveError = leaveFromEdit()
			if left then
				if waitForEnd(3) then
					return { stopped = true, job = playtestJobSnapshot(job), via = "StudioTestService.LeaveTest" }
				end
				leaveError = "Studio accepted LeaveTest but the playtest remained active"
			end
			if not serverContext then
				restoreRetryableState()
				error("unable to force-stop playtest: " .. tostring(leaveError))
			end
		end

		if serverContext then
			local requested, requestError = pcall(function()
				requestRuntimeContext(serverContext, "end_test", { result = args.result }, if forceStop then 1 else 5)
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
			restoreRetryableState()
			error(
				"unable to stop playtest through PlayServer ("
					.. tostring(requestError)
					.. ") or edit mode ("
					.. tostring(leaveError)
					.. ")"
			)
		end

		local left, leaveError = leaveFromEdit()
		if not left then
			restoreRetryableState()
			error("unable to stop playtest: " .. tostring(leaveError))
		end
		if not waitForEnd(3) then
			restoreRetryableState()
			error("Studio accepted LeaveTest but the playtest remained active")
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
		local forceStop = terminal.force == true or terminal.outcome == "timeout" or terminal.outcome == "bootFailure"
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
			playscriptRunTombstones[clientRunId] = playscriptRunTombstones[clientRunId] or { createdAt = os.clock() }
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
		local fn, compileErr = loadstring(src, "rosync-transmit")
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
		local moduleScript = script:FindFirstChild("Photo")
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
						local corner =
							part.CFrame:PointToWorldSpace(Vector3.new(half.X * xSign, half.Y * ySign, half.Z * zSign))
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
		local photoChild = script:FindFirstChild("Photo")
		local photoAvailable = photoChild ~= nil and photoChild:IsA("ModuleScript")
		local playscriptChild = script:FindFirstChild("Playscript")
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
		local photoChild = script:FindFirstChild("Photo")
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
			restoreCamera = focusCameraForCapture(focusPath, tostring(args.view or "isometric"), tonumber(args.padding))
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
			methods = { "Destroy", "Clone", "IsA", "FindFirstChild", "GetChildren", "GetDescendants", "GetFullName" },
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
		elseif lower:find("missing", 1, true) or lower:find("invalid", 1, true) or lower:find("must be", 1, true) then
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

	local function sendSyntheticRemoteFailure(item, code: string, message: string, retryable: boolean, details: any?)
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

			item.started = true
			item.executionStartedAt = os.clock()
			remoteWriteActive = true
			local responseBound = remoteWriteResponseBound(item)
			task.delay(responseBound, function()
				if item.started and not item.finished and not item.responded then
					item.executionTimedOut = true
					sendSyntheticRemoteFailure(
						item,
						"HANDLER_TIMEOUT",
						string.format(
							"write handler %s exceeded the %.0fs response bound; it remains serialized until it returns",
							item.op,
							responseBound
						),
						true
					)
				end
			end)

			local ok, result = pcall(item.handler, item.args)
			remoteWriteActive = false
			finishRemoteRequest(item, ok, result)

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

local applyWallyConfig

reconnectState.stopTerminal = function(reason)
	local message = tostring(reason or "connection rejected")
	reconnectState.desired = false
	reconnectState.compareRetryPending = false
	connected = false
	busy = false
	wsGeneration += 1
	decisionGeneration += 1
	disconnectHooks()
	reconnectState.pluginCapability = nil
	reconnectState.comparedCapability = nil
	if ws then
		local toClose = ws
		ws = nil
		pcall(function()
			toClose:Close()
		end)
	end
	setPill("error", "update Ro Sync")
	if message:lower():find("protocol", 1, true) then
		setBanner("Daemon/plugin versions are incompatible. Update Ro Sync and reinstall the Studio plugin.")
	else
		setBanner("Ro Sync connection rejected: " .. message)
	end
	log("ws: terminal rejection — " .. message)
end

-- A daemon restart rotates the process-scoped plugin capability. Refresh it
-- immediately before every WebSocket attempt, and rediscover the matching
-- project daemon when Desktop had to move it to another port.
reconnectState.refreshHello = function()
	local function readHello(candidateUrl)
		if type(candidateUrl) ~= "string" or candidateUrl == "" then
			return nil, "daemon URL is missing"
		end
		local ok, response = httpRequestTo(candidateUrl:gsub("/+$", "") .. "/hello", "GET")
		if not ok then
			return nil, tostring(response)
		end
		if not response.Success then
			return nil,
				string.format("HTTP %s %s", tostring(response.StatusCode), tostring(response.StatusMessage or ""))
		end
		local decoded = nil
		local decodeOk = pcall(function()
			decoded = HttpService:JSONDecode(response.Body or "")
		end)
		if not decodeOk or type(decoded) ~= "table" then
			return nil, "daemon hello was malformed"
		end
		return decoded, nil
	end

	local expectedGameId = tostring(game.GameId)
	local candidateUrl = tostring(baseUrl or ""):gsub("/+$", "")
	local candidatePort = connectedPort
	local hello, helloError = readHello(candidateUrl)
	if hello ~= nil and expectedGameId ~= "0" and tostring(hello.gameId or "") ~= expectedGameId then
		hello = nil
		helloError = "daemon now belongs to a different GameId"
	end

	if hello == nil and type(reconnectState.discoverDaemon) == "function" and expectedGameId ~= "0" then
		local discovered = reconnectState.discoverDaemon(expectedGameId, true, false)
		if type(discovered) == "table" and discovered.found == true then
			candidateUrl = tostring(discovered.url or ""):gsub("/+$", "")
			candidatePort = tostring(discovered.port or candidatePort or "?")
			hello, helloError = readHello(candidateUrl)
		end
	end

	if hello == nil then
		return false, "daemon hello unavailable: " .. tostring(helloError or "not found"), false
	end
	if expectedGameId ~= "0" and tostring(hello.gameId or "") ~= expectedGameId then
		return false, "daemon hello changed to a different GameId during discovery", false
	end
	local protocol = tonumber(hello.pluginProtocol)
	if protocol ~= PLUGIN_PROTOCOL_VERSION then
		return false,
			string.format(
				"daemon protocol %s is incompatible; expected %d",
				tostring(protocol or "missing"),
				PLUGIN_PROTOCOL_VERSION
			),
			true
	end
	local capability = hello.pluginCapability
	if type(capability) ~= "string" or #capability ~= 64 or not capability:match("^[0-9a-fA-F]+$") then
		return false, "daemon hello omitted a valid plugin capability", false
	end

	baseUrl = candidateUrl
	connectedPort = candidatePort or candidateUrl:match(":(%d+)$") or "?"
	reconnectState.pluginCapability = capability
	applyWallyConfig(hello.wallyEnabled, hello.wallyFolder)
	if ui and ui.setProjectName and type(hello.name) == "string" and hello.name ~= "" then
		ui.setProjectName(hello.name)
	end
	return true, nil, false
end

local function wsLoop(gen)
	local clientId = tostring(game.GameId)
	while connected and gen == wsGeneration do
		local helloOk, helloError, terminalHelloError = reconnectState.refreshHello()
		if not reconnectState.desired or not connected or gen ~= wsGeneration then
			return
		end
		if not helloOk and terminalHelloError then
			reconnectState.stopTerminal(helloError)
			return
		end
		local recoveryContext = "WebSocket transport unavailable after initial comparison"
		if helloOk and reconnectState.comparedCapability ~= reconnectState.pluginCapability then
			helloOk = false
			helloError = "daemon identity changed after initial comparison"
			recoveryContext = helloError
		end
		local client
		local createOk = false
		local createErr = helloError
		-- CreateWebStreamClient takes a CONFIG TABLE, not a bare URL string.
		-- Roblox rejects arg 2 with "unable to cast string to dictionary" if a
		-- string is passed. The documented field is `Url`.
		if helloOk then
			local url = toWsUrl(baseUrl)
			log("ws: connecting to " .. url)
			createOk, createErr = pcall(function()
				client = HttpService:CreateWebStreamClient(Enum.WebStreamClientType.WebSocket, { Url = url })
			end)
		else
			log("ws: " .. tostring(helloError))
			setPill("connecting")
		end
		-- Hooks are already live after /initial-compare. Any failed or rejected
		-- transport attempt can therefore straddle unseen filesystem events, even
		-- when the daemon never acknowledged our hello. Fail closed into another
		-- full comparison instead of reconnecting this socket in place.
		local needResyncAfter = true
		if not createOk or not client then
			log("ws: create err: " .. tostring(createErr))
			setPill("connecting")
		else
			local closed = false
			local accepted = false
			local terminalReason = nil
			local lastPongClock = os.clock()
			local conns: { RBXScriptConnection } = {}
			local function acceptTransport()
				if accepted or not reconnectState.desired or not connected or gen ~= wsGeneration then
					if not reconnectState.desired or not connected or gen ~= wsGeneration then
						pcall(function()
							client:Close()
						end)
					end
					return
				end
				accepted = true
				ws = client
				wsBackoff = WS_BACKOFF_START
				setPill("connected", tostring(connectedPort or "?"))
				setBanner(nil)
				log("ws: authenticated — live sync resumed")
				schedulePushRetry(0)
			end

			table.insert(
				conns,
				client.MessageReceived:Connect(function(payload)
					if not reconnectState.desired or not connected or gen ~= wsGeneration then
						pcall(function()
							client:Close()
						end)
						return
					end
					local msg = parseWsPayload(payload)
					if not msg then
						return
					end
					-- Any server traffic counts as liveness for the heartbeat
					-- watchdog — the daemon pings every 10s on its own schedule,
					-- so we don't need to see a pong specifically.
					lastPongClock = os.clock()
					local kind = tostring(msg.type or "")
					if
						kind == "op"
						or kind == "lagged"
						or kind == "pong"
						or kind == "ping"
						or kind == "push-result"
						or kind == "request"
					then
						acceptTransport()
					end
					if kind == "op" then
						-- Daemon wraps the op under msg.op (nested, per Agent 1).
						if type(msg.op) == "table" then
							applyOps({ msg.op })
							-- Echo the source Studio actually accepted. The daemon uses
							-- this as the acknowledgement that advances its conflict
							-- baseline; failed UpdateSourceAsync calls produce no ack.
							local ack = sourceAckForAppliedOp(msg.op)
							if ack then
								wsSend(client, { type = "push", ops = { ack } })
							end
						else
							log("ws: op frame missing nested op payload")
						end
					elseif kind == "lagged" then
						log("ws: daemon broadcast lagged — will resync via /initial-compare")
						recoveryContext = "daemon broadcast lagged"
						pcall(function()
							client:Close()
						end)
					elseif kind == "shutdown" then
						local reason = tostring(msg.reason or "daemon requested reconnect")
						local retryable = msg.retryable
						recoveryContext = "daemon rejected WebSocket: " .. tostring(msg.code or reason)
						local lowered = reason:lower()
						if
							retryable == false
							or (lowered:find("incompatible", 1, true) and lowered:find("protocol", 1, true))
						then
							terminalReason = reason
						end
						log(
							string.format(
								"ws: daemon shutdown%s — %s",
								msg.code and " [" .. tostring(msg.code) .. "]" or "",
								reason
							)
						)
						pcall(function()
							client:Close()
						end)
					elseif kind == "pong" then
					-- liveness already refreshed above
					elseif kind == "ping" then
						wsSend(client, { type = "pong" })
					elseif kind == "push-result" then
						if msg.ok == false or (msg.errors and #msg.errors > 0) then
							log(
								string.format(
									"ws push-result: ok=%s applied=%s skipped=%s errs=%s",
									tostring(msg.ok),
									tostring(msg.applied),
									tostring(msg.skipped),
									tostring(msg.errors and #msg.errors or 0)
								)
							)
						end
					elseif kind == "request" then
						remoteState.handleRemoteRequest(client, msg)
					elseif kind == "error" then
						log("ws error: " .. tostring(msg.error or "?"))
					elseif kind == "config-changed" then
						applyWallyConfig(msg.wallyEnabled, msg.wallyFolder)
						log("ws event: " .. kind)
					elseif kind == "conflict" or kind == "initial-choice-needed" or kind == "initial-choice-made" then
						-- Events forwarded from daemon.state.events. The plugin's
						-- initial-sync path uses HTTP /initial-decision polling, so
						-- these are informational here.
						log("ws event: " .. kind)
					elseif kind == "plugin" then
					-- Daemon status fan-out for widget clients; Studio already
					-- knows whether this socket is connected.
					else
						log("ws: unknown msg type " .. tostring(kind))
					end
				end)
			)
			table.insert(
				conns,
				client.Closed:Connect(function()
					closed = true
				end)
			)

			-- Announce ourselves. If the initial Send fails outright, the common
			-- recovery path below re-runs the full comparison with backoff.
			local helloOk = wsSend(client, {
				type = "hello",
				clientId = clientId,
				role = "plugin",
				protocol = PLUGIN_PROTOCOL_VERSION,
				pluginCapability = reconnectState.pluginCapability,
			})
			if helloOk then
				lastPongClock = os.clock()
				-- A pong is the daemon's acknowledgement that it accepted our
				-- authenticated hello. Keep `ws` unpublished until then so queued
				-- filesystem pushes cannot race ahead of authentication.
				if not wsSend(client, { type = "ping" }) then
					closed = true
					pcall(function()
						client:Close()
					end)
				end
				local lastPing = os.clock()
				while connected and gen == wsGeneration and not closed do
					task.wait(1)
					local now = os.clock()
					if now - lastPing >= WS_HEARTBEAT_INTERVAL then
						lastPing = now
						wsSend(client, { type = "ping" })
					end
					if now - lastPongClock > WS_HEARTBEAT_TIMEOUT then
						log("ws: heartbeat timeout — closing")
						pcall(function()
							client:Close()
						end)
						break
					end
				end
			else
				log("ws: hello send failed")
			end

			for _, c in ipairs(conns) do
				pcall(function()
					c:Disconnect()
				end)
			end
			pcall(function()
				client:Close()
			end)
			-- A workflow transaction is owned by the remote session. Losing that
			-- session makes commit intent unknowable, so fail closed and roll back.
			remoteState.requestRemoteWriteCleanup("disconnect")
			if ws == client then
				ws = nil
			end
			if terminalReason ~= nil then
				reconnectState.stopTerminal(terminalReason)
				return
			end
			if accepted and recoveryContext == "WebSocket transport unavailable after initial comparison" then
				recoveryContext = "authenticated WebSocket closed"
			end
		end

		if not connected or gen ~= wsGeneration then
			break
		end

		if needResyncAfter then
			-- Tear down Studio hooks and re-enter the full initial-sync handshake.
			-- Bumping wsGeneration ensures any other in-flight wsLoop iteration
			-- exits. Pending point ops are represented by the fresh Studio snapshot,
			-- so discard them before a disk-win decision can accidentally replay one.
			log("ws: transport gap — scheduling full /initial-compare")
			disconnectHooks()
			pendingOps = {}
			pushPending = false
			connected = false
			busy = true
			reconnectState.pluginCapability = nil
			reconnectState.comparedCapability = nil
			wsGeneration += 1
			setPill("connecting")
			reconnectState.retryInitialCompare(recoveryContext)
			return
		end
	end
end

reconnectState.retryInitialCompare = function(context)
	if not reconnectState.desired then
		busy = false
		return
	end
	if reconnectState.compareRetryPending then
		return
	end
	reconnectState.compareRetryPending = true
	busy = true
	setPill("connecting")
	setBanner("Connection interrupted. Ro Sync is waiting for the matching daemon and will resync automatically.")
	local generation = wsGeneration
	local waitSec = wsBackoff
	wsBackoff = math.min(wsBackoff * 2, WS_BACKOFF_MAX)
	log(string.format("recovery: %s; retrying initial sync in %ds", tostring(context or "transport error"), waitSec))
	task.delay(waitSec, function()
		reconnectState.compareRetryPending = false
		if not reconnectState.desired or generation ~= wsGeneration or not busy then
			return
		end
		local helloOk, helloError, terminalHelloError = reconnectState.refreshHello()
		if not reconnectState.desired or generation ~= wsGeneration or not busy then
			return
		end
		if not helloOk then
			if terminalHelloError then
				reconnectState.stopTerminal(helloError)
			else
				reconnectState.retryInitialCompare(helloError)
			end
			return
		end
		runInitialCompare()
	end)
end

local function startHooksAndWs()
	if not reconnectState.desired or not busy then
		return
	end
	installHooks()
	reconnectState.compareRetryPending = false
	connected = true
	busy = false
	wsGeneration += 1
	local gen = wsGeneration
	setPill("connecting")
	setBanner("Initial sync complete. Authenticating the live Studio connection...")
	log("initial sync complete — watching " .. #WATCHED_SERVICES .. " services; authenticating WebSocket")
	task.spawn(function()
		wsLoop(gen)
	end)
	-- Seed the daemon's live tree cache with the current Studio skeleton. Spawned so the HTTP
	-- POST doesn't block the hook-installation path. Subsequent updates are
	-- debounced through markTreeDirty from onDescendantAdded/Removing/Changed.
	treeDirty = true
	task.spawn(pushTreeSkeleton)
end

local function doPushPath(strictStudio: boolean?)
	log("initial-compare → push: sending Studio snapshot")
	local services = collectSnapshot()
	local strict = strictStudio == true

	-- Push one service per HTTP call. A monolithic bootstrap body of N
	-- services could easily exceed Studio's 30s RequestAsync timeout on a
	-- big project; if that timed out mid-write the daemon could end up with
	-- a partially-written disk, which then (on a later pull) could try to
	-- re-parent Studio services as Folders. Splitting per-service keeps
	-- every call small, and any one failure only costs that service.
	local okCount, skipCount = 0, 0
	local firstErr = nil
	for _, svc in ipairs(services) do
		if not reconnectState.desired or not busy then
			return false
		end
		local svcName = (svc and svc.name) or "?"
		local encOk, encErr = pcall(function()
			return HttpService:JSONEncode(svc)
		end)
		if not encOk then
			log(string.format("  ✗ %s — cannot encode (%s); skipping", svcName, tostring(encErr):sub(1, 200)))
			skipCount += 1
		else
			local ok, err = httpJson("/push", "POST", {
				bootstrap = true,
				strict = strict,
				forcePrune = strict,
				pluginProtocol = PLUGIN_PROTOCOL_VERSION,
				services = { svc },
			})
			if not reconnectState.desired or not busy then
				return false
			end
			if ok then
				okCount += 1
			else
				skipCount += 1
				firstErr = firstErr or tostring(err)
				log(string.format("  ✗ %s — push err: %s", svcName, tostring(err):sub(1, 200)))
			end
		end
	end

	if okCount == 0 or firstErr ~= nil then
		setPill("error", "push failed")
		log(
			string.format(
				"bootstrap incomplete: %d/%d services pushed (first err: %s)",
				okCount,
				#services,
				tostring(firstErr):sub(1, 200)
			)
		)
		return false
	end

	log(string.format("bootstrap done: %d/%d services pushed (%d skipped)", okCount, #services, skipCount))
	return true
end

local function doPullPath(strictDisk: boolean?)
	log("initial-compare → pull: applying disk snapshot to Studio")
	local snapshotPath = strictDisk == true and "/snapshot?strict=true&forcePrune=true" or "/snapshot"
	local ok, snap = httpJson(snapshotPath, "GET")
	if not reconnectState.desired or not busy then
		return false
	end
	if not ok then
		log("snapshot err: " .. tostring(snap))
		setPill("error", "snapshot failed")
		return false
	end
	applySnapshot(snap)
	return true
end

-- A daemon or Desktop broker may advertise the authorized project initializer.
-- Keep this state near the connection UI that consumes it so the plugin's very
-- large top-level chunk can reuse earlier compiler registers.
local projectInitCandidate: any = nil
local connectionAttempt = 0

local function disconnect(reason)
	reconnectState.desired = false
	reconnectState.compareRetryPending = false
	connected = false
	busy = false
	wsGeneration += 1
	decisionGeneration += 1
	disconnectHooks()
	remoteState.requestRemoteWriteCleanup()
	if ws then
		local toClose = ws
		ws = nil
		pcall(function()
			toClose:Close()
		end)
	end
	setPill("idle")
	setBanner(nil)
	connectedPort = nil
	reconnectState.pluginCapability = nil
	reconnectState.comparedCapability = nil
	projectInitCandidate = nil
	connectionAttempt += 1
	if reason then
		log("disconnected: " .. reason)
	end
end

--------------------------------------------------------------------------------
-- Initial-sync decision handshake
--------------------------------------------------------------------------------

local function waitForDecision(choiceId, gen)
	setPill("waiting")
	setBanner(string.format("Waiting for overwrite decision… (choice %s)", tostring(choiceId)))
	while busy and gen == decisionGeneration do
		local url = string.format("%s/initial-decision?choiceId=%s", baseUrl, HttpService:UrlEncode(tostring(choiceId)))
		local ok, resp = httpRequestTo(url, "GET")
		if gen ~= decisionGeneration then
			return nil
		end
		if not ok then
			log("decision poll err: " .. tostring(resp))
			local helloOk, helloError, terminalHelloError = reconnectState.refreshHello()
			if gen ~= decisionGeneration or not busy then
				return nil
			end
			if terminalHelloError then
				reconnectState.stopTerminal(helloError)
				return nil
			elseif not helloOk then
				log("decision recovery: " .. tostring(helloError))
			end
			task.wait(2)
		elseif not resp.Success then
			if resp.StatusCode == 408 or resp.StatusCode == 204 then
				-- idle timeout, re-issue
			elseif resp.StatusCode == 404 then
				log(string.format("decision HTTP 404 for stale choice %s; re-comparing", tostring(choiceId)))
				return "stale"
			else
				log(string.format("decision HTTP %d", resp.StatusCode))
				task.wait(2)
			end
		else
			local decoded
			local decOk = pcall(function()
				decoded = HttpService:JSONDecode(resp.Body or "")
			end)
			if decOk and decoded then
				if decoded.pending == true then
					-- keep polling
				elseif decoded.choice then
					return tostring(decoded.choice)
				end
			end
		end
	end
	return nil
end

local function countArray(value)
	if type(value) ~= "table" then
		return 0
	end
	return #value
end

local function describeInitialComparison(comparison)
	if type(comparison) ~= "table" then
		return "synced-path summary unavailable"
	end

	local summary = comparison.summary
	if type(summary) == "table" then
		local newFiles = tonumber(summary.newFiles) or 0
		local changedFiles = tonumber(summary.changedFiles) or 0
		local removedFiles = tonumber(summary.removedFiles) or 0
		return string.format("%d new, %d changed, %d removed synced paths", newFiles, changedFiles, removedFiles)
	end

	return string.format(
		"%d new, %d changed, %d removed synced paths",
		countArray(comparison.newFiles),
		countArray(comparison.changedFiles),
		countArray(comparison.removedFiles)
	)
end

-- Assigned to the forward-declared local at the top of the HTTP helpers
-- section so wsLoop's transport-gap recovery can reach it without a circular
-- declaration.
runInitialCompare = function()
	local compareGeneration = wsGeneration
	local compareCapability = reconnectState.pluginCapability
	local function completeInitialCompare()
		if not reconnectState.desired or not busy or compareGeneration ~= wsGeneration then
			return
		end
		reconnectState.comparedCapability = compareCapability
		startHooksAndWs()
	end
	local studioStats = collectStudioStats()
	local studioSnapshot = collectSnapshot()
	if not reconnectState.desired or not busy or compareGeneration ~= wsGeneration then
		return
	end
	log("initial-compare: comparing synced paths")
	local ok, resp = httpJson("/initial-compare", "POST", {
		studioStats = studioStats,
		studioSnapshot = studioSnapshot,
		pluginProtocol = PLUGIN_PROTOCOL_VERSION,
	})
	if not reconnectState.desired or not busy or compareGeneration ~= wsGeneration then
		return
	end
	if not ok then
		log("initial-compare err: " .. tostring(resp))
		reconnectState.retryInitialCompare(resp)
		return
	end
	if type(resp) ~= "table" or not resp.action then
		log("initial-compare: malformed response")
		reconnectState.stopTerminal("daemon returned a malformed initial-compare response")
		return
	end
	local action = tostring(resp.action)
	if action == "in-sync" then
		-- Daemon says disk and Studio already match within tolerance — skip
		-- the overwrite prompt and start hooks directly.
		log("initial-compare: already in sync, skipping decision modal")
		completeInitialCompare()
	elseif action == "push" then
		if doPushPath() then
			completeInitialCompare()
		else
			reconnectState.retryInitialCompare("snapshot push failed")
		end
	elseif action == "pull" then
		if doPullPath() then
			completeInitialCompare()
		else
			reconnectState.retryInitialCompare("snapshot pull failed")
		end
	elseif action == "decide" then
		local choiceId = resp.choiceId
		log("decide: " .. describeInitialComparison(resp.comparison))
		decisionGeneration += 1
		local gen = decisionGeneration
		task.spawn(function()
			local choice = waitForDecision(choiceId, gen)
			if gen ~= decisionGeneration then
				return
			end
			if choice == "disk" then
				if doPullPath(true) then
					completeInitialCompare()
				else
					reconnectState.retryInitialCompare("strict snapshot pull failed")
				end
			elseif choice == "studio" then
				if doPushPath(true) then
					completeInitialCompare()
				else
					reconnectState.retryInitialCompare("strict snapshot push failed")
				end
			elseif choice == "cancel" then
				log("decision: cancel")
				disconnect("cancelled by user")
			elseif choice == "stale" then
				log("decision: stale choice; restarting initial compare")
				busy = true
				setPill("connecting")
				setBanner(nil)
				runInitialCompare()
			else
				log("decision: unknown/aborted")
				disconnect("decision aborted")
			end
		end)
	else
		log("initial-compare: unknown action " .. action)
		reconnectState.stopTerminal("daemon returned an unknown initial-compare action: " .. action)
	end
end

--------------------------------------------------------------------------------
-- Port probing / auto-discovery
--------------------------------------------------------------------------------

local function trimTrailingSlash(s)
	local trimmed = tostring(s or ""):match("^%s*(.-)%s*$") or ""
	return (trimmed:gsub("/+$", ""))
end

local function probePort(port, gameId)
	-- Returns (matched: bool, url: string?, hello: table?)
	local url = string.format("http://127.0.0.1:%d", port)
	local ok, resp = httpRequestTo(url .. "/hello", "GET")
	if not ok or not resp.Success then
		return false, url, nil
	end
	local hello
	local decOk = pcall(function()
		hello = HttpService:JSONDecode(resp.Body or "")
	end)
	if not decOk or type(hello) ~= "table" then
		return false, url, nil
	end
	local helloGame = tostring(hello.gameId or "")
	if helloGame == gameId then
		return true, url, hello
	end
	return false, url, hello
end

applyWallyConfig = function(wallyEnabled, wallyFolder)
	wallyRootSegments = nil
	if wallyEnabled == true and type(wallyFolder) == "string" and wallyFolder ~= "" then
		local segments = splitStudioPath(wallyFolder)
		wallyRootSegments = segments
		log("wally generated root: " .. table.concat(segments, "/"))
	end
end

local function refreshDaemonConfig(url)
	wallyRootSegments = nil
	local ok, resp = httpRequestTo(trimTrailingSlash(url) .. "/hello", "GET")
	if not ok or not resp.Success then
		return
	end
	local hello
	local decOk = pcall(function()
		hello = HttpService:JSONDecode(resp.Body or "")
	end)
	if not decOk or type(hello) ~= "table" then
		return
	end
	applyWallyConfig(hello.wallyEnabled, hello.wallyFolder)
	if ui and ui.setProjectName and type(hello.name) == "string" and hello.name ~= "" then
		ui.setProjectName(hello.name)
	end
end

local function validateDaemonProtocol(url)
	local ok, resp = httpRequestTo(trimTrailingSlash(url) .. "/hello", "GET")
	if not ok or not resp.Success then
		return false, "daemon hello failed"
	end
	local hello
	local decOk = pcall(function()
		hello = HttpService:JSONDecode(resp.Body or "")
	end)
	if not decOk or type(hello) ~= "table" then
		return false, "daemon hello was malformed"
	end
	local protocol = tonumber(hello.pluginProtocol)
	if protocol ~= PLUGIN_PROTOCOL_VERSION then
		return false,
			string.format(
				"daemon protocol %s is incompatible; expected %d",
				tostring(protocol or "missing"),
				PLUGIN_PROTOCOL_VERSION
			)
	end
	local capability = hello.pluginCapability
	if type(capability) ~= "string" or #capability ~= 64 or not capability:match("^[0-9a-fA-F]+$") then
		return false, "daemon hello omitted a valid plugin capability"
	end
	return true, nil, capability
end

-- Project-bootstrap constants live in this late connection scope instead of at
-- the top of the already-large plugin chunk, preserving Luau's 200-register
-- compiler budget for the earlier capture and playtest function declarations.
local BROKER_PORT_START = 7867
local BROKER_PORT_END = 7870
local PROJECT_DAEMON_WAIT_SECONDS = 120

local function projectInitOffer(url, port, hello)
	if type(hello) ~= "table" or tonumber(hello.pluginProtocol) ~= PLUGIN_PROTOCOL_VERSION then
		return nil
	end
	local capability = hello.pluginCapability
	if type(capability) ~= "string" or #capability ~= 64 or not capability:match("^[0-9a-fA-F]+$") then
		return nil
	end
	local init = hello.projectInit
	if type(init) ~= "table" or init.available ~= true then
		return nil
	end
	local projectsRoot = init.projectsRoot
	local endpoint = init.endpoint
	if type(projectsRoot) ~= "string" or projectsRoot == "" then
		return nil
	end
	if
		type(endpoint) ~= "string"
		or #endpoint > 128
		or not endpoint:match("^/[A-Za-z0-9_%%/%.%-]+$")
		or endpoint:sub(1, 2) == "//"
		or endpoint:find("..", 1, true) ~= nil
	then
		return nil
	end
	return {
		url = url,
		port = port,
		endpoint = endpoint,
		projectsRoot = projectsRoot,
		pluginCapability = capability,
	}
end

local function projectInitUnavailableMessage(hello)
	if type(hello) ~= "table" then
		return nil
	end
	local init = hello.projectInit
	if type(init) ~= "table" or init.broker ~= true or init.available == true then
		return nil
	end
	if type(init.error) == "string" and init.error ~= "" then
		return init.error
	end
	return "Set a Projects folder in Ro Sync Desktop Settings before creating this project."
end

reconnectState.discoverDaemon = function(gameId, quiet, includeInitializer)
	-- Probe concurrently, but choose the lowest-port exact match and initializer
	-- deterministically after all completed probes have been collected.
	if not quiet then
		setPill("probing")
		log(string.format("probing ports %d..%d for GameId %s", PORT_RANGE_START, PORT_RANGE_END, gameId))
	end
	local result = {
		found = false,
		url = nil,
		port = nil,
		sawWrongGame = false,
		initializer = nil,
		initializerError = nil,
	}
	local function probeRange(firstPort, lastPort)
		local probes = {}
		local remaining = lastPort - firstPort + 1
		local closed = false
		for port = firstPort, lastPort do
			task.spawn(function()
				local matched, url, hello = probePort(port, gameId)
				if not closed then
					probes[port] = { matched = matched, url = url, hello = hello }
				end
				remaining -= 1
			end)
		end
		local started = os.clock()
		while remaining > 0 and (os.clock() - started) < PROBE_TIMEOUT_SECONDS do
			task.wait(0.05)
		end
		closed = true
		return probes
	end

	local probes = probeRange(PORT_RANGE_START, PORT_RANGE_END)
	local daemonInitializer = nil

	for port = PORT_RANGE_START, PORT_RANGE_END do
		local probe = probes[port]
		if probe then
			if probe.matched and not result.found then
				result.found = true
				result.url = probe.url
				result.port = port
			elseif probe.hello ~= nil then
				result.sawWrongGame = true
			end
			if daemonInitializer == nil and probe.hello ~= nil then
				daemonInitializer = projectInitOffer(probe.url, port, probe.hello)
			end
		end
	end

	-- The Desktop broker is deliberately outside the project-daemon range. It
	-- exists even when there are no projects yet, so inspect it only after no
	-- matching GameId daemon was found. Prefer it over a wrong-game daemon's
	-- initializer because Desktop owns the configured projects root and daemon
	-- lifecycle handoff.
	if not result.found and includeInitializer ~= false then
		if not quiet then
			log(string.format("probing Desktop broker ports %d..%d", BROKER_PORT_START, BROKER_PORT_END))
		end
		local brokerProbes = probeRange(BROKER_PORT_START, BROKER_PORT_END)
		for port = BROKER_PORT_START, BROKER_PORT_END do
			local probe = brokerProbes[port]
			if probe and probe.hello ~= nil then
				local offer = projectInitOffer(probe.url, port, probe.hello)
				if offer ~= nil then
					result.initializer = offer
					break
				elseif result.initializerError == nil then
					result.initializerError = projectInitUnavailableMessage(probe.hello)
				end
			end
		end
	end
	if result.initializer == nil then
		result.initializer = daemonInitializer
	end
	return result
end

local function positiveIdString(value)
	local text = tostring(value or ""):match("^%s*(.-)%s*$") or ""
	if text == "" or not text:match("^%d+$") or text:match("^0+$") then
		return nil
	end
	return (text:gsub("^0+", ""))
end

local function normalizeCreatorType(value)
	local text = tostring(value or "")
	if text == "Group" or text:match("%.Group$") then
		return "Group"
	elseif text == "User" or text:match("%.User$") then
		return "User"
	end
	return nil
end

local function collectProjectInitMetadata()
	local gameName = tostring(game.Name or "")
	local loweredGameName = gameName:lower()
	local genericGameName = gameName == ""
		or gameName:match("^Place%d*$") ~= nil
		or loweredGameName == "game"
		or loweredGameName == "untitled experience"
	if gameName == "" then
		gameName = "Untitled Experience"
	end
	local placeName = gameName
	local creatorType = nil
	local creatorId = nil

	pcall(function()
		creatorType = normalizeCreatorType(game.CreatorType)
		creatorId = positiveIdString(game.CreatorId)
	end)

	-- Product info gives Studio a better place label and fills creator metadata
	-- on channels where DataModel.CreatorType/CreatorId are unavailable.
	local infoOk, info = pcall(function()
		return game:GetService("MarketplaceService"):GetProductInfo(game.PlaceId, Enum.InfoType.Asset)
	end)
	if infoOk and type(info) == "table" then
		if type(info.Name) == "string" and info.Name ~= "" then
			placeName = info.Name
			-- DataModel.Name is commonly the generic "Place1" even for a
			-- published experience. Product info is the authoritative visible
			-- title in that case, so do not create projects named Place1.
			if genericGameName then
				gameName = info.Name
			end
		end
		local creator = info.Creator
		if type(creator) == "table" then
			creatorType = normalizeCreatorType(creator.CreatorType or creator.Type) or creatorType
			creatorId = positiveIdString(creator.CreatorTargetId or creator.Id or creator.CreatorId) or creatorId
		end
	end

	local metadata = {
		gameName = gameName,
		placeName = placeName,
		gameId = positiveIdString(game.GameId),
		placeId = positiveIdString(game.PlaceId),
		creatorType = creatorType,
		creatorId = creatorId,
	}
	if creatorType == "Group" then
		metadata.groupId = creatorId
	end
	return metadata
end

local function beginDaemonConnection(chosenUrl, chosenPort)
	local protocolOk, protocolError, daemonPluginCapability = validateDaemonProtocol(chosenUrl)
	if not protocolOk then
		reconnectState.desired = false
		log("connect blocked: " .. tostring(protocolError))
		setPill("error", "update Ro Sync")
		setBanner("Daemon/plugin versions are incompatible. Update Ro Sync and reinstall the Studio plugin.")
		busy = false
		return false
	end

	projectInitCandidate = nil
	reconnectState.desired = true
	reconnectState.pluginCapability = daemonPluginCapability
	reconnectState.comparedCapability = nil
	wsBackoff = WS_BACKOFF_START
	baseUrl = trimTrailingSlash(chosenUrl)
	connectedPort = tostring(chosenPort or "?")
	refreshDaemonConfig(baseUrl)
	setPill("connecting")
	setBanner(nil)

	-- Initial-sync handshake before hooks + ws.
	runInitialCompare()
	return true
end

local function projectInitErrorMessage(decoded)
	if type(decoded) ~= "table" then
		return "the daemon returned a malformed project response"
	end
	local err = decoded.error
	if type(err) == "table" then
		local message = tostring(err.message or err.code or "project creation failed")
		if type(err.suggestedDirectoryName) == "string" and err.suggestedDirectoryName ~= "" then
			message ..= " Suggested folder: " .. err.suggestedDirectoryName
		end
		return message
	end
	return tostring(err or "project creation failed")
end

local function createProjectAndWait(candidate, attempt)
	setPill("project_creating")
	setBanner("Collecting Studio metadata and creating the Ro Sync project...")
	local metadata = collectProjectInitMetadata()
	if metadata.gameId == nil or metadata.placeId == nil then
		setPill("error", "publish this place first")
		setBanner("Ro Sync needs a published GameId and PlaceId before it can create a project.")
		busy = false
		projectInitCandidate = nil
		return
	end

	log(
		string.format(
			"requesting project initialization for %s (GameId %s, PlaceId %s)",
			metadata.gameName,
			metadata.gameId,
			metadata.placeId
		)
	)
	local ok, response = httpRequestTo(candidate.url .. candidate.endpoint, "POST", {
		pluginCapability = candidate.pluginCapability,
		requestId = HttpService:GenerateGUID(false),
		gameName = metadata.gameName,
		placeName = metadata.placeName,
		gameId = metadata.gameId,
		placeId = metadata.placeId,
		creatorType = metadata.creatorType,
		creatorId = metadata.creatorId,
		groupId = metadata.groupId,
	})
	if attempt ~= connectionAttempt or not busy then
		return
	end
	if not ok then
		setPill("error", "project request failed")
		setBanner("Could not reach Ro Sync Desktop: " .. tostring(response))
		log("project-init request failed: " .. tostring(response))
		busy = false
		projectInitCandidate = nil
		return
	end

	local decoded = nil
	local decodeOk = pcall(function()
		decoded = HttpService:JSONDecode(response.Body or "")
	end)
	if not response.Success or not decodeOk or type(decoded) ~= "table" or decoded.ok ~= true then
		local message = projectInitErrorMessage(decoded)
		setPill("error", "project creation failed")
		setBanner(message)
		log(string.format("project-init rejected (HTTP %s): %s", tostring(response.StatusCode), message))
		busy = false
		projectInitCandidate = nil
		return
	end

	projectInitCandidate = nil
	local projectPath =
		tostring(decoded.project or decoded.projectPath or decoded.directoryName or "the configured projects folder")
	local initStatus = decoded.status or (decoded.reused == true and "existing" or "created")
	log(string.format("project-init %s: %s", tostring(initStatus), projectPath))
	setPill("project_waiting")
	setBanner("Project created at " .. projectPath .. ". Waiting for Ro Sync Desktop to start its daemon...")

	local deadline = os.clock() + PROJECT_DAEMON_WAIT_SECONDS
	while busy and attempt == connectionAttempt and os.clock() < deadline do
		local result = reconnectState.discoverDaemon(metadata.gameId, true, false)
		if attempt ~= connectionAttempt or not busy then
			return
		end
		if result.found then
			log(string.format("Desktop daemon ready at %s", tostring(result.url)))
			beginDaemonConnection(result.url, result.port)
			return
		end
		task.wait(1)
	end

	if attempt == connectionAttempt and busy then
		busy = false
		setPill("no_daemon", metadata.gameId)
		setBanner(
			"The project was created at "
				.. projectPath
				.. ", but Desktop did not start its daemon. Open Ro Sync Desktop, then click Connect."
		)
		log("project-init: timed out waiting for the Desktop-managed daemon")
	end
end

--------------------------------------------------------------------------------
-- Connect / Disconnect entry point
--------------------------------------------------------------------------------

startConnect = function()
	if connected then
		disconnect("user")
		return
	end
	if busy then
		-- Cancel in-flight probing/connecting/waiting
		reconnectState.desired = false
		reconnectState.compareRetryPending = false
		reconnectState.pluginCapability = nil
		reconnectState.comparedCapability = nil
		busy = false
		wsGeneration += 1
		decisionGeneration += 1
		connectionAttempt += 1
		projectInitCandidate = nil
		setPill("idle")
		setBanner(nil)
		log("cancelled")
		return
	end

	if projectInitCandidate ~= nil then
		local candidate = projectInitCandidate
		projectInitCandidate = nil
		busy = true
		connectionAttempt += 1
		local attempt = connectionAttempt
		task.spawn(function()
			createProjectAndWait(candidate, attempt)
		end)
		return
	end

	busy = true
	connectionAttempt += 1
	local attempt = connectionAttempt

	-- Persist manual URL if the user typed one
	local manualUrl = urlBox.Text
	if manualUrl == nil then
		manualUrl = ""
	end
	manualUrl = trimTrailingSlash(manualUrl)
	plugin:SetSetting(SETTING_MANUAL_URL, manualUrl)

	local gameId = tostring(game.GameId)
	log("game.GameId = " .. gameId)

	task.spawn(function()
		local chosenUrl = nil
		local chosenPort = nil

		if manualUrl ~= "" then
			-- Manual override takes precedence
			chosenUrl = manualUrl
			log("using manual URL: " .. chosenUrl)
			-- Best-effort /hello for port label
			local ok, resp = httpRequestTo(chosenUrl .. "/hello", "GET")
			if ok and resp.Success then
				local hello
				pcall(function()
					hello = HttpService:JSONDecode(resp.Body or "")
				end)
				if type(hello) == "table" and hello.gameId then
					if gameId ~= "0" and tostring(hello.gameId) ~= gameId then
						log(
							string.format(
								"warning: manual URL reports gameId=%s but Studio is %s",
								tostring(hello.gameId),
								gameId
							)
						)
						setPill("wrong_game")
						busy = false
						return
					end
				end
			end
			-- Try to extract port from URL for label
			chosenPort = chosenUrl:match(":(%d+)") or "?"
		elseif gameId == "0" then
			log("GameId is 0 (unpublished place). Prompting for manual URL.")
			setPill("idle")
			setBanner("Publish your place first, or enter a manual daemon URL.")
			local resolved = nil
			local waiting = true
			showUnpublishedModal(function(result)
				resolved = result
				waiting = false
			end)
			while waiting do
				task.wait(0.05)
				if attempt ~= connectionAttempt or not busy then
					return
				end
			end
			setBanner(nil)
			if resolved == nil or resolved == "" then
				log("unpublished-place modal cancelled")
				setPill("idle")
				busy = false
				return
			end
			local typed = trimTrailingSlash(resolved)
			urlBox.Text = typed
			plugin:SetSetting(SETTING_MANUAL_URL, typed)
			chosenUrl = typed
			chosenPort = typed:match(":(%d+)") or "?"
			log("using manual URL from modal: " .. typed)
		else
			local result = reconnectState.discoverDaemon(gameId, false)
			if attempt ~= connectionAttempt or not busy then
				return
			end
			if not result.found then
				if result.initializer ~= nil then
					projectInitCandidate = result.initializer
					setPill("project_init")
					setBanner(
						"No Ro Sync project exists for this experience. Click Create Project to initialize it under "
							.. tostring(result.initializer.projectsRoot)
							.. "."
					)
					log(string.format("project initializer available at %s", tostring(result.initializer.url)))
				elseif result.initializerError ~= nil then
					setPill("no_daemon", gameId)
					setBanner(tostring(result.initializerError))
					log("Desktop project initializer unavailable: " .. tostring(result.initializerError))
				elseif result.sawWrongGame then
					setPill("wrong_game")
					setBanner("Ro Sync Desktop is running, but its daemon does not offer project initialization.")
					log("found a daemon on a probed port, but no gameId match or project initializer")
				else
					setPill("no_daemon", gameId)
				end
				busy = false
				return
			end
			chosenUrl = result.url
			chosenPort = tostring(result.port)
			log(string.format("matched daemon at %s", chosenUrl))
		end

		if attempt ~= connectionAttempt or not busy then
			return
		end
		beginDaemonConnection(chosenUrl, chosenPort)
	end)
end

plugin.Unloading:Connect(function()
	disconnect("unloading")
	if ui and ui.destroy then
		ui.destroy()
	end
end)

setPill("idle")
log("Ro Sync loaded. Click Connect to auto-discover the daemon.")
