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
local DAEMON_STARTUP_GRACE_SECONDS = 30
local DAEMON_DISCOVERY_RETRY_SECONDS = 1
local DAEMON_PROBE_CONCURRENCY = 4
local MAX_LOG_LINES = 200
local SOURCE_WRITE_CONCURRENCY = 16
local PROTOCOL_STREAM_MAX_BYTES = 512 * 1024
-- Live point updates share the same bounded wire budget as protocol-6
-- bootstrap chunks. Larger subtrees are recovered through a fresh streamed
-- comparison instead of constructing an unbounded WebSocket frame.
local LIVE_PUSH_MAX_BYTES = PROTOCOL_STREAM_MAX_BYTES
local RO_SYNC_LOGO_IMAGE = "rbxassetid://134480165435830"
local RO_SYNC_SETTINGS_IMAGE = "rbxassetid://128477924144442"

-- Identifier returned by `rosync version` / `ping`. Bump when breaking changes
-- to the remoteHandlers protocol ship.
local PLUGIN_VERSION_STRING = "2.4.1"
local PLUGIN_PROTOCOL_VERSION = 6
local PLUGIN_BUILD_COMMIT = "unknown"
local PLUGIN_BUILD_DIRTY = false
pcall(function()
	local module = script:FindFirstChild("BuildInfo")
	local info = if module and module:IsA("ModuleScript") then require(module) else nil
	if type(info) == "table" then
		if type(info.commit) == "string" and info.commit ~= "" then
			PLUGIN_BUILD_COMMIT = info.commit
		end
		PLUGIN_BUILD_DIRTY = info.dirty == true
	end
end)

--------------------------------------------------------------------------------
-- Playtest runtime agent
--
-- Studio clones this plugin into PlayServer/PlayClient DataModels. Those
-- copies must not start the edit-mode UI or connect to the localhost daemon.
-- Instead, each one exposes a small, structured agent over
-- PluginConnectionService. The edit-mode copy is the sole external bridge.
--------------------------------------------------------------------------------

local function hostDataModelTypeName(): (string, string, boolean, boolean)
	local name = "Unknown"
	pcall(function()
		name = plugin.HostDataModelType.Name
	end)

	-- Connection direction is the public, authoritative way to distinguish
	-- edit and test DataModels. In particular, Run mode can expose an "Edit"
	-- HostDataModelType from its runtime plugin clone; trusting that value first
	-- causes the clone to start the edit UI and never advertise a server context.
	local canConnectToEdit = false
	local canConnectToTest = false
	local inspectedConnections = pcall(function()
		local connectionService = game:GetService("PluginConnectionService")
		canConnectToEdit = connectionService:CanHaveConnectionType(Enum.PluginConnectionTargetType.Edit)
		canConnectToTest = connectionService:CanHaveConnectionType(Enum.PluginConnectionTargetType.Test)
	end)
	if inspectedConnections and canConnectToTest then
		return "Edit", name, canConnectToEdit, canConnectToTest
	end
	if inspectedConnections and canConnectToEdit then
		local runService = game:GetService("RunService")
		return if runService:IsServer() then "PlayServer" else "PlayClient", name, canConnectToEdit, canConnectToTest
	end

	-- HostDataModelType is unavailable in some Studio channels, so keep it as a
	-- compatibility fallback after the connection-direction check.
	if name == "PlayServer" or name == "PlayClient" then
		return name, name, canConnectToEdit, canConnectToTest
	end

	local runService = game:GetService("RunService")
	if runService:IsRunning() then
		return if runService:IsServer() then "PlayServer" else "PlayClient", name, canConnectToEdit, canConnectToTest
	end
	return "Edit", name, canConnectToEdit, canConnectToTest
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
	-- Studio can clone and start the runtime plugin before Run-mode test args
	-- become observable. Give that handoff a short grace period instead of
	-- permanently disabling the bridge after the first empty GetTestArgs().
	local testArgsDeadline = os.clock() + 5
	repeat
		pcall(function()
			local testArgs = game:GetService("StudioTestService"):GetTestArgs()
			if type(testArgs) == "table" then
				runtimeSessionToken = tostring(testArgs.__rosyncSessionToken or "")
				runtimePlaytestJobId = tostring(testArgs.__rosyncJobId or "")
			end
		end)
		if runtimeSessionToken ~= "" and runtimePlaytestJobId ~= "" then
			break
		end
		task.wait(0.05)
	until os.clock() >= testArgsDeadline
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
		local logicalHostDataModelType, rawHostDataModelType, canConnectToEdit, canConnectToTest =
			hostDataModelTypeName()
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
			hostDataModelType = logicalHostDataModelType,
			rawHostDataModelType = rawHostDataModelType,
			canConnectToEdit = canConnectToEdit,
			canConnectToTest = canConnectToTest,
			testArgsAuthenticated = runtimeSessionToken ~= "" and runtimePlaytestJobId ~= "",
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
		elseif state == "reconnecting" then
			status.Text = string.format("Retrying in %ss...", tostring(detail or "?"))
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
		elseif state == "daemon_waiting" then
			status.Text = string.format("Waiting for Desktop... %ss", tostring(detail or "?"))
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
local function hasSyncableDescendant(inst)
	if isAvoidSyncBlocked(inst) then
		return false
	end
	local stack = inst:GetChildren()
	while #stack > 0 do
		local child = table.remove(stack)
		if not SUPPRESS_CLASSES[child.ClassName] and not hasAvoidSyncAttribute(child) then
			if child:IsA("LuaSourceContainer") then
				return true
			end
			for _, descendant in ipairs(child:GetChildren()) do
				stack[#stack + 1] = descendant
			end
		end
	end
	return false
end

-- True if an instance needs a presence on disk — either it's directly ALLOWED,
-- or it's a pass-through container that has at least one ALLOWED descendant.
local function isSyncable(inst)
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

local function tryReadScriptSource(inst): (string?, string?)
	if ScriptEditorService then
		local ok, src = pcall(function()
			return ScriptEditorService:GetEditorSource(inst)
		end)
		if ok and type(src) == "string" then
			return src, nil
		end
		return nil, "ScriptEditorService:GetEditorSource failed: " .. tostring(src)
	end
	local source = safeGet(inst, "Source")
	if type(source) == "string" then
		return source, nil
	end
	return nil, "script Source is unavailable"
end

-- Non-authoritative reads (CLI inspection and detached staging validation) may
-- still use the raw Source property when ScriptEditorService cannot address an
-- Instance. Authoritative Studio -> disk paths use tryReadScriptSource
-- directly and abort coherently instead of manufacturing an empty Source.
local function readScriptSource(inst): string
	local source, sourceErr = tryReadScriptSource(inst)
	if source ~= nil then
		return source
	end
	local fallback = safeGet(inst, "Source")
	if type(fallback) == "string" then
		return fallback
	end
	error(sourceErr or "script Source is unavailable", 0)
end

-- Post-scope-down: only Source (for scripts) round-trips. Every other property
-- is Studio-authoritative — we never serialize it, never push it, never apply
-- it. Non-script nodes get `properties:{}` purely for schema stability so the
-- daemon's op deserializer never sees a missing field.
local function serializeProperties(inst)
	if inst:IsA("LuaSourceContainer") then
		local source, sourceErr = tryReadScriptSource(inst)
		if source == nil then
			error(sourceErr or "script Source is unavailable", 0)
		end
		return { Source = source }
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
local function serializeInstance(inst, shouldContinue)
	if isAvoidSyncBlocked(inst) then
		return nil
	end
	local result = nil
	local estimatedBytes = 0
	local visited = 0
	local stack = {
		{
			inst = inst,
			entered = false,
		},
	}
	while #stack > 0 do
		local frame = stack[#stack]
		if not frame.entered then
			frame.entered = true
			frame.nextChild = 1
			frame.outputChildren = {}
			frame.hasSyncableChild = false
			visited += 1
			if visited % 500 == 0 then
				task.wait()
				if shouldContinue and not shouldContinue() then
					return nil, "cancelled"
				end
			end
			if SUPPRESS_CLASSES[frame.inst.ClassName] or hasAvoidSyncAttribute(frame.inst) then
				frame.children = {}
				frame.suppressed = true
			else
				frame.children = frame.inst:GetChildren()
			end
		end

		local child = frame.children[frame.nextChild]
		if child then
			frame.nextChild += 1
			if not SUPPRESS_CLASSES[child.ClassName] then
				stack[#stack + 1] = {
					inst = child,
					entered = false,
				}
			end
		else
			local isScript = frame.inst:IsA("LuaSourceContainer")
			local syncable = not frame.suppressed and (isScript or frame.hasSyncableChild)
			local node = nil
			if syncable then
				local allowed = ALLOWED_CLASSES[frame.inst.ClassName] == true
				local properties = {}
				if allowed and isScript then
					local source, sourceErr = tryReadScriptSource(frame.inst)
					if source == nil then
						return nil, "Source unavailable for " .. frame.inst:GetFullName() .. ": " .. tostring(sourceErr)
					end
					estimatedBytes += #source
					if estimatedBytes > LIVE_PUSH_MAX_BYTES then
						return nil, "oversized"
					end
					properties.Source = source
				end
				estimatedBytes += #frame.inst.Name + #frame.inst.ClassName + 16
				if estimatedBytes > LIVE_PUSH_MAX_BYTES then
					return nil, "oversized"
				end
				node = {
					class = allowed and frame.inst.ClassName or "Folder",
					name = frame.inst.Name,
					properties = properties,
					children = frame.outputChildren,
				}
				if frame.inst.ClassName == "Folder" or not allowed then
					materializedContainers[frame.inst] = true
				end
			end
			table.remove(stack)
			local parent = stack[#stack]
			if parent then
				if node then
					parent.outputChildren[#parent.outputChildren + 1] = node
				end
				if syncable then
					parent.hasSyncableChild = true
				end
			else
				result = node
			end
		end
	end
	return result
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
		segs[#segs + 1] = cur.Name
		cur = cur.Parent
	end
	for i = 1, math.floor(#segs / 2) do
		local opposite = #segs - i + 1
		segs[i], segs[opposite] = segs[opposite], segs[i]
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

local snapshotApplyState
local PathHelpers
do
	local moduleScript = script:FindFirstChild("PathHelpers")
	if not moduleScript or not moduleScript:IsA("ModuleScript") then
		error("[Ro Sync] packaged PathHelpers module is missing")
	end
	local module = require(moduleScript)
	PathHelpers = module.install({
		scriptClasses = SCRIPT_CLASSES,
		getSnapshotApplyState = function()
			return snapshotApplyState
		end,
	})
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
	if not wallyRootSegments or #wallyRootSegments == 0 then
		return false
	end
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

local function resolveGeneratedPath(path, ctx)
	if type(path) ~= "table" then
		return nil
	end
	local cur = game
	ctx = ctx or snapshotApplyState.newContext({
		indexAvoidSyncCarriers = true,
	})
	for _, seg in ipairs(path) do
		local nxt
		if cur == game then
			local ok, svc = pcall(function()
				return game:GetService(seg)
			end)
			if ok and svc then
				nxt = svc
			else
				nxt = PathHelpers.findGeneratedPathChild(cur, seg, ctx)
					or PathHelpers.findRawDisambiguatedChild(cur, seg)
					or cur:FindFirstChild(seg)
			end
		else
			-- Diff/selective paths use generated duplicate suffixes. Resolve
			-- that identity before a literal same-name Roblox child.
			nxt = PathHelpers.findGeneratedPathChild(cur, seg, ctx)
				or PathHelpers.findRawDisambiguatedChild(cur, seg)
				or cur:FindFirstChild(seg)
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
local sourceSyncState = {
	pending = {},
	expected = {},
	generation = {},
}

local function isEchoing(): boolean
	return (os.clock() - lastRemoteApplyClock) < ECHO_GUARD_WINDOW
end

local function normalizeSourceForCompare(source: string): string
	if not source:find("\r\n", 1, true) then
		return source
	end
	-- The wire contract normalizes CRLF only. A lone CR is Source content,
	-- not an alternate newline spelling, and must never compare equal to LF.
	return (source:gsub("\r\n", "\n"))
end

local function sourcesMatchForApply(currentSource: string, newSource: string): boolean
	if currentSource == newSource then
		return true
	end
	return normalizeSourceForCompare(currentSource) == normalizeSourceForCompare(newSource)
end

function sourceSyncState.expectRemoteSource(inst: Instance, source: string): number
	local generation = (sourceSyncState.generation[inst] or 0) + 1
	sourceSyncState.generation[inst] = generation
	-- A filesystem write supersedes editor text captured before this exact
	-- apply. The delayed callback validates this generation again at commit.
	sourceSyncState.pending[inst] = nil
	sourceSyncState.expected[inst] = {
		sourceHash = sha256Hex(source),
		normalizedHash = sha256Hex(normalizeSourceForCompare(source)),
		generation = generation,
	}
	return generation
end

function sourceSyncState.clearExpected(inst: Instance, generation: number)
	local expected = sourceSyncState.expected[inst]
	if expected and expected.generation == generation then
		sourceSyncState.expected[inst] = nil
	end
end

function sourceSyncState.consumeExpected(inst: Instance, observed: string): boolean
	local expected = sourceSyncState.expected[inst]
	if not expected then
		return false
	end
	local observedHash = sha256Hex(observed)
	local matches = observedHash == expected.sourceHash
		or sha256Hex(normalizeSourceForCompare(observed)) == expected.normalizedHash
	if matches then
		sourceSyncState.pending[inst] = nil
		return true
	end
	-- A differing editor value is a real user edit after the remote apply.
	sourceSyncState.expected[inst] = nil
	return false
end

local function applyScriptSource(inst, newSource: string, suppressStudioEcho: boolean?): boolean
	-- Only filesystem-originated writes are echoes. Remote CLI writes are
	-- Studio-originated mutations and must be allowed through the normal
	-- Studio -> disk hooks. Treating them as daemon echoes loses the deferred
	-- ChildAdded/Changed signal entirely (most visibly for `rosync new` with an
	-- initial Source), leaving the new script live-only until reconnect.
	local suppressEcho = suppressStudioEcho ~= false
	local sourceGeneration = if suppressEcho then sourceSyncState.expectRemoteSource(inst, newSource) else nil
	if suppressEcho then
		lastRemoteApplyClock = os.clock()
	end
	if not ScriptEditorService then
		if sourceGeneration then
			sourceSyncState.clearExpected(inst, sourceGeneration)
		end
		warn("[Ro Sync] ScriptEditorService unavailable; source write was not applied")
		return false
	end
	local ok, err = pcall(function()
		snapshotApplyState.runInternalWrite(function()
			ScriptEditorService:UpdateSourceAsync(inst, function(_old)
				return newSource
			end)
		end, {
			instance = inst,
			source = true,
			sourceValue = newSource,
		})
	end)
	if suppressEcho then
		lastRemoteApplyClock = os.clock()
	end
	if not ok then
		if sourceGeneration then
			sourceSyncState.clearExpected(inst, sourceGeneration)
		end
		warn("[Ro Sync] UpdateSourceAsync failed; source write was not applied: " .. tostring(err))
	end
	return ok
end

local function writeScriptSource(inst, newSource: string, suppressStudioEcho: boolean?): boolean
	-- Equality is authoritative only when it came from the live editor
	-- document. The raw Source property can lag an open editor buffer; if that
	-- read is unavailable, force UpdateSourceAsync and return its real result.
	local currentSource = tryReadScriptSource(inst)
	if currentSource ~= nil and sourcesMatchForApply(currentSource, newSource) then
		if suppressStudioEcho ~= false then
			sourceSyncState.expectRemoteSource(inst, newSource)
		end
		return true
	end
	return applyScriptSource(inst, newSource, suppressStudioEcho)
end

local function queueScriptSourceWrite(queue, inst, newSource: string, op): boolean
	-- Always retain the desired value until every operation in this drain has
	-- been seen. Comparing to Studio here is unsafe because an earlier queued
	-- write for the same Script has not run yet (A -> B -> A would otherwise
	-- skip the final A and leave B).
	table.insert(queue, {
		inst = inst,
		source = newSource,
		-- Which inbound op produced this write, so a single rejected Source can be
		-- reported against that op alone instead of failing its whole batch.
		op = op,
	})
	return true
end

local function flushScriptSourceWrites(queue, shouldContinue, failedJobs): (number, number)
	-- Preserve concurrency across different scripts while making repeated
	-- writes to one Script strictly last-write-wins. Without this coalescing,
	-- two workers could complete in reverse order and leave Studio with the
	-- older Source even though the newer filesystem op was acknowledged.
	local coalesced = {}
	local indexByInstance = {}
	for _, job in ipairs(queue) do
		local existingIndex = indexByInstance[job.inst]
		if existingIndex then
			coalesced[existingIndex] = job
		else
			table.insert(coalesced, job)
			indexByInstance[job.inst] = #coalesced
		end
	end
	queue = coalesced
	local pending = {}
	for _, job in ipairs(queue) do
		local currentSource = tryReadScriptSource(job.inst)
		if currentSource ~= nil and sourcesMatchForApply(currentSource, job.source) then
			sourceSyncState.expectRemoteSource(job.inst, job.source)
		else
			table.insert(pending, job)
		end
	end
	queue = pending
	local total = #queue
	if total == 0 then
		return 0, 0
	end

	local nextIndex = 0
	local finishedWorkers = 0
	local workers = math.min(SOURCE_WRITE_CONCURRENCY, total)
	local applied = 0

	for _ = 1, workers do
		task.spawn(function()
			while true do
				if shouldContinue and not shouldContinue() then
					break
				end
				nextIndex += 1
				local job = queue[nextIndex]
				if not job then
					break
				end
				if applyScriptSource(job.inst, job.source) then
					applied += 1
				elseif failedJobs then
					table.insert(failedJobs, job)
				end
			end
			finishedWorkers += 1
		end)
	end

	while finishedWorkers < workers do
		lastRemoteApplyClock = os.clock()
		task.wait()
	end

	return applied, total - applied
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
			queueScriptSourceWrite(opts.sourceWrites, inst, source, opts.currentOp)
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
	local stack = inst:GetChildren()
	while #stack > 0 do
		local child = table.remove(stack)
		n += 1
		if n >= cap then
			return n
		end
		for _, descendant in ipairs(child:GetChildren()) do
			stack[#stack + 1] = descendant
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

-- Disk snapshots can contain tens of thousands of nodes and duplicate Roblox
-- sibling names. Keep the implementation behind one table (this top-level
-- chunk is close to Luau's register limit), use explicit DFS frames, and match
-- children by Instance identity rather than repeatedly calling FindFirstChild.
snapshotApplyState = {
	stepsPerCheck = 64,
	sliceSeconds = 0.01,
	diskPathCache = setmetatable({}, { __mode = "k" }),
	activeMutationGuard = nil,
}

function snapshotApplyState.runInternalWrite(callback, expectedMutation)
	-- A streamed pull keeps a mutation guard installed while UpdateSourceAsync
	-- may yield. Scope suppression to the exact Instance/event being authored;
	-- a process-global depth flag would hide unrelated user edits during that
	-- yield and could incorrectly commit over them.
	local guard = snapshotApplyState.activeMutationGuard
	if guard and expectedMutation and guard.pushExpectedMutation then
		guard.pushExpectedMutation(expectedMutation)
	end
	local packed = table.pack(pcall(callback))
	if guard and expectedMutation and guard.popExpectedMutation then
		guard.popExpectedMutation(expectedMutation)
	end
	if not packed[1] then
		error(packed[2], 0)
	end
	return table.unpack(packed, 2, packed.n)
end

function snapshotApplyState.clearDiskPathSubtree(root: Instance)
	local stack = { root }
	while #stack > 0 do
		local current = table.remove(stack)
		snapshotApplyState.diskPathCache[current] = nil
		for _, child in ipairs(current:GetChildren()) do
			stack[#stack + 1] = child
		end
	end
end

function snapshotApplyState.seedDiskPathAncestry(inst: Instance, diskPath)
	if type(diskPath) ~= "table" then
		return
	end
	local current = inst
	local depth = #diskPath
	while current and current ~= game and depth >= 1 do
		local prefix = {}
		for index = 1, depth do
			prefix[index] = diskPath[index]
		end
		snapshotApplyState.diskPathCache[current] = prefix
		current = current.Parent
		depth -= 1
	end
end

function snapshotApplyState.newContext(opts)
	return {
		steps = 0,
		lastYield = os.clock(),
		shouldContinue = opts and opts.shouldContinue,
		claimedInstances = (opts and opts.claimedInstances) or {},
		avoidSyncCarriers = {},
		avoidSyncBoundaries = {},
		syncableInstances = {},
		directoryInstances = {},
		sortKeys = {},
		indexedServices = {},
		useAvoidSyncIndex = opts and opts.indexAvoidSyncCarriers == true,
	}
end

function snapshotApplyState.nodeOptions(shared, strict, forcePrune)
	local opts = {
		strict = strict == true,
		forcePrune = forcePrune == true,
	}
	if shared then
		opts.sourceWrites = shared.sourceWrites
		opts.shouldContinue = shared.shouldContinue
		opts.claimedInstances = shared.claimedInstances
		opts._applyContext = shared._applyContext
		opts.indexAvoidSyncCarriers = shared.indexAvoidSyncCarriers
	end
	return opts
end

function snapshotApplyState.checkpoint(ctx)
	ctx.steps += 1
	if ctx.steps % snapshotApplyState.stepsPerCheck ~= 0 then
		return
	end

	lastRemoteApplyClock = os.clock()
	if ctx.shouldContinue and not ctx.shouldContinue() then
		error("snapshot apply cancelled", 0)
	end
	if os.clock() - ctx.lastYield >= snapshotApplyState.sliceSeconds then
		task.wait()
		lastRemoteApplyClock = os.clock()
		ctx.lastYield = os.clock()
		if ctx.shouldContinue and not ctx.shouldContinue() then
			error("snapshot apply cancelled", 0)
		end
	end
end

function snapshotApplyState.takeCandidate(listState, used, claimed)
	if not listState then
		return nil
	end
	while listState.next <= #listState.items do
		local candidate = listState.items[listState.next]
		listState.next += 1
		if not used[candidate] and not claimed[candidate] then
			used[candidate] = true
			return candidate
		end
	end
	return nil
end

function snapshotApplyState.indexAvoidSyncCarriers(parent, ctx)
	local service = parent
	while service and service.Parent ~= game do
		service = service.Parent
	end
	if not service or ctx.indexedServices[service] then
		return
	end
	ctx.indexedServices[service] = true

	local stack = {
		{
			inst = service,
			entered = false,
			hasSyncableChild = false,
			hasAvoidSyncChild = false,
			projectionChildKeys = {},
		},
	}
	while #stack > 0 do
		snapshotApplyState.checkpoint(ctx)
		local frame = stack[#stack]
		if not frame.entered then
			if SUPPRESS_CLASSES[frame.inst.ClassName] then
				table.remove(stack)
			elseif hasAvoidSyncAttribute(frame.inst) then
				ctx.avoidSyncBoundaries[frame.inst] = true
				if not frame.inst:IsA("LuaSourceContainer") or #frame.inst:GetChildren() > 0 then
					ctx.directoryInstances[frame.inst] = true
				end
				local mappedClass = PathHelpers.mappedSyncClass(frame.inst)
				ctx.sortKeys[frame.inst] = table.concat({
					string.lower(frame.inst.Name),
					frame.inst.Name,
					mappedClass,
					mappedClass,
				}, "\0")
				table.remove(stack)
				local ancestor = stack[#stack]
				if ancestor then
					ancestor.hasAvoidSyncChild = true
					ancestor.projectionChildKeys[#ancestor.projectionChildKeys + 1] = ctx.sortKeys[frame.inst]
				end
			else
				frame.entered = true
				frame.children = frame.inst:GetChildren()
				frame.nextChild = 1
			end
		else
			local child = frame.children[frame.nextChild]
			if child then
				frame.nextChild += 1
				stack[#stack + 1] = {
					inst = child,
					entered = false,
					hasSyncableChild = false,
					hasAvoidSyncChild = false,
					projectionChildKeys = {},
				}
			else
				local syncable = frame.inst:IsA("LuaSourceContainer") or frame.hasSyncableChild
				local hasProjectedChild = frame.hasSyncableChild or frame.hasAvoidSyncChild
				if syncable then
					ctx.syncableInstances[frame.inst] = true
				elseif frame.inst ~= service and frame.hasAvoidSyncChild then
					ctx.avoidSyncCarriers[frame.inst] = true
				end
				if
					frame.inst ~= service
					and (not frame.inst:IsA("LuaSourceContainer") or hasProjectedChild)
					and (syncable or frame.hasAvoidSyncChild)
				then
					ctx.directoryInstances[frame.inst] = true
				end
				if frame.inst ~= service and (syncable or frame.hasAvoidSyncChild) then
					table.sort(frame.projectionChildKeys)
					local mappedClass = PathHelpers.mappedSyncClass(frame.inst)
					local signature = { mappedClass }
					for _, key in ipairs(frame.projectionChildKeys) do
						signature[#signature + 1] = key
					end
					ctx.sortKeys[frame.inst] = table.concat({
						string.lower(frame.inst.Name),
						frame.inst.Name,
						mappedClass,
						table.concat(signature, "\0"),
					}, "\0")
				end
				local carriesAvoidSync = frame.hasAvoidSyncChild
				table.remove(stack)
				local ancestor = stack[#stack]
				if ancestor then
					if syncable then
						ancestor.hasSyncableChild = true
					end
					if carriesAvoidSync then
						ancestor.hasAvoidSyncChild = true
					end
					local sortKey = ctx.sortKeys[frame.inst]
					if sortKey then
						ancestor.projectionChildKeys[#ancestor.projectionChildKeys + 1] = sortKey
					end
				end
			end
		end
	end
end

function snapshotApplyState.projectedCandidateInfo(child, ctx)
	local boundary = ctx.avoidSyncBoundaries[child] == true or hasAvoidSyncAttribute(child)
	local carrier = ctx.avoidSyncCarriers[child] == true
	local syncable = ctx.syncableInstances[child] == true
	local directory = ctx.directoryInstances[child] == true
	if not ctx.useAvoidSyncIndex then
		local hasProjectedChild = false
		if not boundary then
			for _, descendant in ipairs(child:GetChildren()) do
				if hasAvoidSyncAttribute(descendant) or isSyncable(descendant) then
					hasProjectedChild = true
					break
				end
			end
		end
		syncable = child:IsA("LuaSourceContainer") or hasProjectedChild
		carrier = not syncable and hasProjectedChild
		directory = (not child:IsA("LuaSourceContainer") or hasProjectedChild) and (syncable or carrier)
	end
	if not boundary and not carrier and not syncable then
		return nil
	end

	local mappedClass = PathHelpers.mappedSyncClass(child)
	return {
		mappedClass = mappedClass,
		-- AvoidSync markers intentionally omit their descendants. The daemon
		-- reserves a leaf first for ignored scripts, then the alternate
		-- directory shape. Folder/carrier projections are always directories.
		directory = if boundary and SCRIPT_CLASSES[mappedClass] then false else directory,
		boundary = boundary,
		reserved = boundary or carrier,
		key = ctx.sortKeys[child] or PathHelpers.sortKeyWithName(nil, child.Name, mappedClass),
	}
end

function snapshotApplyState.buildPhysicalSiblingIndex(parent, ctx, override)
	if ctx.useAvoidSyncIndex then
		snapshotApplyState.indexAvoidSyncCarriers(parent, ctx)
	end
	local projectionEntries = {}
	for childIndex, child in ipairs(parent:GetChildren()) do
		snapshotApplyState.checkpoint(ctx)
		if not SUPPRESS_CLASSES[child.ClassName] then
			local projection = snapshotApplyState.projectedCandidateInfo(child, ctx)
			if projection then
				local projectedName = child.Name
				if override and override.instance == child then
					projectedName = override.name or projectedName
					if type(override.directory) == "boolean" then
						projection.directory = override.directory
					end
					if type(override.key) == "string" then
						projection.key = override.key
					elseif projectedName ~= child.Name then
						projection.key =
							PathHelpers.sortKeyWithName(projection.key, projectedName, projection.mappedClass)
					end
				end
				projectionEntries[#projectionEntries + 1] = {
					child = child,
					name = projectedName,
					projection = projection,
					reserved = projection.reserved,
					key = projection.key,
					index = childIndex,
				}
			end
		end
	end
	table.sort(projectionEntries, function(left, right)
		if left.reserved ~= right.reserved then
			return left.reserved
		end
		if left.key ~= right.key then
			return left.key < right.key
		end
		-- Rust's stable sibling sort retains the Studio GetChildren order for
		-- indistinguishable leaves. Lua's table.sort is not stable, so make the
		-- same tie-break explicit.
		return left.index < right.index
	end)

	-- GetChildren order is not durable across Studio sessions. Only when the
	-- structural keys are truly tied, use normalized Source content to give
	-- otherwise indistinguishable script siblings the same order as Rust's
	-- snapshot diff. Existing disk-path cache entries still override the fresh
	-- allocation below, so a later Source edit cannot reshuffle live identity.
	local groupStart = 1
	while groupStart <= #projectionEntries do
		local groupEnd = groupStart
		local first = projectionEntries[groupStart]
		while
			groupEnd + 1 <= #projectionEntries
			and projectionEntries[groupEnd + 1].reserved == first.reserved
			and projectionEntries[groupEnd + 1].key == first.key
		do
			groupEnd += 1
		end
		if groupEnd > groupStart and SCRIPT_CLASSES[first.projection.mappedClass] then
			local tied = {}
			local complete = true
			for index = groupStart, groupEnd do
				local entry = projectionEntries[index]
				local source = tryReadScriptSource(entry.child)
				if source == nil then
					complete = false
					break
				end
				tied[#tied + 1] = {
					entry = entry,
					sourceKey = sha256Hex((source:gsub("\r\n", "\n"))),
				}
			end
			if complete then
				table.sort(tied, function(left, right)
					if left.sourceKey ~= right.sourceKey then
						return left.sourceKey < right.sourceKey
					end
					return left.entry.index < right.entry.index
				end)
				for offset, item in ipairs(tied) do
					projectionEntries[groupStart + offset - 1] = item.entry
				end
			end
		end
		groupStart = groupEnd + 1
	end

	local allocator = {
		taken = {},
		nextOrdinal = {},
	}
	local logicalAllocator = {
		taken = {},
		nextOrdinal = {},
	}
	-- Exact disk identities are durable once assigned. Reserve every usable
	-- same-parent cache entry before allocating a fragment for a new sibling;
	-- otherwise a newly inserted duplicate whose Source sorts first can take
	-- the cached sibling's base fragment and make both Instances target the
	-- same file.
	local cachedFragmentByInstance = {}
	local cachedFragmentOwner = {}
	local parentDiskPath = snapshotApplyState.diskPathCache[parent]
	if parentDiskPath and not override then
		for _, entry in ipairs(projectionEntries) do
			local cached = snapshotApplyState.diskPathCache[entry.child]
			local sameParent = cached and #cached == #parentDiskPath + 1
			if sameParent then
				for index, fragment in ipairs(parentDiskPath) do
					if cached[index] ~= fragment then
						sameParent = false
						break
					end
				end
			end
			if sameParent then
				local fragment = cached[#cached]
				local usable = type(fragment) == "string"
					and fragment ~= ""
					and not string.find(fragment, "/", 1, true)
					and not string.find(fragment, "\\", 1, true)
				if usable then
					local fragmentKey = string.lower(fragment)
					local owner = cachedFragmentOwner[fragmentKey]
					if not owner then
						cachedFragmentOwner[fragmentKey] = entry.child
						cachedFragmentByInstance[entry.child] = fragment
						allocator.taken[fragmentKey] = true
					elseif owner ~= entry.child then
						-- A prior buggy allocation may have cached the same
						-- physical file for two Instances. Keep the first
						-- canonical claimant and rebuild the losing subtree's
						-- identity instead of silently overwriting a file.
						snapshotApplyState.clearDiskPathSubtree(entry.child)
						warn(
							string.format(
								"[Ro Sync] repaired duplicate cached disk fragment %q under %s",
								fragment,
								parent:GetFullName()
							)
						)
					end
				else
					snapshotApplyState.clearDiskPathSubtree(entry.child)
				end
			end
		end
	end
	local byFragment = {}
	local byInstance = {}
	local byLookupSegment = {}
	local placementOwner = {}
	for physicalOrder, entry in ipairs(projectionEntries) do
		local projection = entry.projection
		local fragment = cachedFragmentByInstance[entry.child]
			or PathHelpers.allocatePhysicalFragment(allocator, entry.name, projection.mappedClass, projection.directory)
		local fragmentKey = string.lower(fragment)
		local priorOwner = placementOwner[fragmentKey]
		if priorOwner and priorOwner ~= entry.child then
			error(string.format("physical sibling fragment collision: %s", fragment), 0)
		end
		placementOwner[fragmentKey] = entry.child
		byFragment[fragmentKey] = entry.child
		local logicalSegment = PathHelpers.allocatePhysicalFragment(logicalAllocator, entry.name, "Folder", true)
		byLookupSegment[logicalSegment] = entry.child
		byInstance[entry.child] = {
			fragment = fragment,
			isDirectory = projection.directory,
			order = physicalOrder,
		}
		if projection.boundary and SCRIPT_CLASSES[projection.mappedClass] then
			local alternate = PathHelpers.allocatePhysicalFragment(
				allocator,
				entry.name,
				projection.mappedClass,
				not projection.directory
			)
			local alternateKey = string.lower(alternate)
			local alternateOwner = placementOwner[alternateKey]
			if alternateOwner and alternateOwner ~= entry.child then
				error(string.format("physical sibling fragment collision: %s", alternate), 0)
			end
			placementOwner[alternateKey] = entry.child
			byFragment[alternateKey] = entry.child
		end
	end
	return {
		byFragment = byFragment,
		byInstance = byInstance,
		byLookupSegment = byLookupSegment,
	}
end

function snapshotApplyState.resolveDiskPath(diskPath, ctx): Instance?
	if type(diskPath) ~= "table" or type(diskPath[1]) ~= "string" or not WATCHED_SET[diskPath[1]] then
		return nil
	end
	local ok, current = pcall(function()
		return game:GetService(diskPath[1])
	end)
	if not ok or not current then
		return nil
	end
	local resolvedPath = { diskPath[1] }
	snapshotApplyState.diskPathCache[current] = resolvedPath
	ctx = ctx or snapshotApplyState.newContext({
		indexAvoidSyncCarriers = true,
	})
	ctx.useAvoidSyncIndex = true
	for index = 2, #diskPath do
		local fragment = diskPath[index]
		if type(fragment) ~= "string" then
			return nil
		end
		local siblings = snapshotApplyState.buildPhysicalSiblingIndex(current, ctx)
		current = siblings.byFragment[string.lower(fragment)]
		if not current then
			return nil
		end
		resolvedPath = table.clone(resolvedPath)
		resolvedPath[#resolvedPath + 1] = fragment
		snapshotApplyState.diskPathCache[current] = resolvedPath
	end
	return current
end

function snapshotApplyState.resolveDiskParent(diskPath, ctx): Instance?
	if type(diskPath) ~= "table" or #diskPath <= 1 then
		return nil
	end
	local parentDiskPath = {}
	for index = 1, #diskPath - 1 do
		parentDiskPath[index] = diskPath[index]
	end
	return snapshotApplyState.resolveDiskPath(parentDiskPath, ctx)
end

function snapshotApplyState.diskPathForInstance(inst: Instance): { string }?
	local cached = snapshotApplyState.diskPathCache[inst]
	if cached then
		return table.clone(cached)
	end
	local ancestry = {}
	local current = inst
	while current and current.Parent ~= game do
		ancestry[#ancestry + 1] = current
		current = current.Parent
	end
	if not current or not WATCHED_SET[current.Name] then
		return nil
	end
	local path = { current.Name }
	snapshotApplyState.diskPathCache[current] = table.clone(path)
	local ctx = snapshotApplyState.newContext({
		indexAvoidSyncCarriers = true,
	})
	for index = #ancestry, 1, -1 do
		local child = ancestry[index]
		local childCached = snapshotApplyState.diskPathCache[child]
		if childCached then
			path = table.clone(childCached)
		else
			local siblings = snapshotApplyState.buildPhysicalSiblingIndex(current, ctx)
			local placement = siblings.byInstance[child]
			if not placement then
				return nil
			end
			for sibling, siblingPlacement in pairs(siblings.byInstance) do
				if not snapshotApplyState.diskPathCache[sibling] then
					local siblingPath = table.clone(path)
					siblingPath[#siblingPath + 1] = siblingPlacement.fragment
					snapshotApplyState.diskPathCache[sibling] = siblingPath
				end
			end
			path = table.clone(path)
			path[#path + 1] = placement.fragment
		end
		current = child
	end
	return path
end

function snapshotApplyState.diskPathForInstanceShape(inst: Instance, isDirectory: boolean): { string }?
	local parent = inst.Parent
	if not parent or parent == game then
		return nil
	end
	local parentPath = snapshotApplyState.diskPathForInstance(parent)
	if not parentPath then
		return nil
	end
	local mappedClass = PathHelpers.mappedSyncClass(inst)
	local ctx = snapshotApplyState.newContext({
		indexAvoidSyncCarriers = true,
	})
	local siblings = snapshotApplyState.buildPhysicalSiblingIndex(parent, ctx)
	local currentPath = snapshotApplyState.diskPathCache[inst]
	local currentFragment = currentPath and currentPath[#currentPath] or nil
	if not currentFragment then
		local placement = siblings.byInstance[inst]
		currentFragment = placement and placement.fragment or nil
	end
	if not currentFragment then
		return nil
	end
	local taken = {}
	for fragmentKey, sibling in pairs(siblings.byFragment) do
		if sibling ~= inst then
			taken[fragmentKey] = true
		end
	end
	local stem = if SCRIPT_CLASSES[mappedClass] then PathHelpers.stripScriptSuffix(currentFragment) else currentFragment
	local encodedDirectoryStem = PathHelpers.encodeName(inst.Name)
	local encodedLeafStem = PathHelpers.leafScriptStem(inst.Name, mappedClass)
	if SCRIPT_CLASSES[mappedClass] and encodedDirectoryStem ~= encodedLeafStem then
		local fromStem = if isDirectory then encodedLeafStem else encodedDirectoryStem
		local toStem = if isDirectory then encodedDirectoryStem else encodedLeafStem
		if stem == fromStem or stem:sub(1, #fromStem + 2) == fromStem .. " [" then
			stem = toStem .. stem:sub(#fromStem + 1)
		end
	end
	local preferred = stem .. (if isDirectory then "" else PathHelpers.scriptSuffix(mappedClass))
	local fragment
	if not taken[string.lower(preferred)] then
		fragment = preferred
	else
		-- Preserve every existing sibling identity. Only if the transitioning
		-- script's matching stem/ordinal is occupied do we allocate a new free
		-- shape against the current physical fragments.
		fragment = PathHelpers.allocatePhysicalFragment({
			taken = taken,
			nextOrdinal = {},
		}, inst.Name, mappedClass, isDirectory)
	end
	local path = table.clone(parentPath)
	path[#path + 1] = fragment
	return path
end

function snapshotApplyState.sourceDiskPathForInstance(inst: Instance): { string }?
	if not inst:IsA("LuaSourceContainer") then
		return nil
	end
	local diskPath = snapshotApplyState.diskPathForInstance(inst)
	if not diskPath then
		return nil
	end
	local parent = inst.Parent
	if not parent or parent == game then
		return diskPath
	end
	local ctx = snapshotApplyState.newContext({
		indexAvoidSyncCarriers = true,
	})
	local siblings = snapshotApplyState.buildPhysicalSiblingIndex(parent, ctx)
	local placement = siblings.byInstance[inst]
	if not placement or not placement.isDirectory then
		return diskPath
	end
	local sourcePath = table.clone(diskPath)
	sourcePath[#sourcePath + 1] = PathHelpers.portableInitFileName(inst.Name, inst.ClassName)
	return sourcePath
end

function snapshotApplyState.seedDiskPathsForService(service: Instance, exactIdentities)
	if exactIdentities then
		-- Initial comparison receipts are daemon-authored from the exact disk
		-- snapshot that was compared. Discard every inferred/stale descendant
		-- identity before installing them.
		snapshotApplyState.clearDiskPathSubtree(service)
	end
	local servicePath = { service.Name }
	snapshotApplyState.diskPathCache[service] = servicePath
	local ctx = snapshotApplyState.newContext({
		indexAvoidSyncCarriers = true,
	})
	local stack = {
		{
			parent = service,
			path = servicePath,
		},
	}
	while #stack > 0 do
		snapshotApplyState.checkpoint(ctx)
		local frame = table.remove(stack)
		if exactIdentities then
			-- Preseed every exact direct-child fragment before allocation so
			-- cached identities reserve their physical filenames and any
			-- Studio-only reservation is placed around them.
			for _, child in ipairs(frame.parent:GetChildren()) do
				local identity = exactIdentities[child]
				if identity then
					local childPath = table.clone(frame.path)
					childPath[#childPath + 1] = identity.fragment
					snapshotApplyState.diskPathCache[child] = childPath
				end
			end
		end
		local siblings = if exactIdentities
			then snapshotApplyState.buildPhysicalSiblingIndex(frame.parent, ctx)
			-- An empty override disables stale cache overlays while retaining
			-- the allocator used for a just-pushed guarded Studio snapshot.
			else snapshotApplyState.buildPhysicalSiblingIndex(frame.parent, ctx, {})
		for child, placement in pairs(siblings.byInstance) do
			local childPath = table.clone(frame.path)
			childPath[#childPath + 1] = placement.fragment
			snapshotApplyState.diskPathCache[child] = childPath
			local exact = exactIdentities and exactIdentities[child]
			if exact and placement.fragment ~= exact.fragment then
				error("daemon-authored disk identity does not match Studio projection", 0)
			end
			-- Shape is deliberately NOT compared. `exact.isDirectory` is the
			-- daemon's physical disk shape while `placement.isDirectory` is the
			-- logical projection, and the two legitimately differ for a script
			-- left in directory form after losing its last child. This runs on
			-- the in-sync path, where a clean comparison means no decision
			-- modal is ever offered — throwing here left the user with no way
			-- forward but Cancel, on every future handshake.
			if placement.isDirectory then
				stack[#stack + 1] = {
					parent = child,
					path = childPath,
				}
			end
		end
	end
end

function snapshotApplyState.buildAssignments(parent, nodes, ctx)
	local buckets = {}
	for _, child in ipairs(parent:GetChildren()) do
		snapshotApplyState.checkpoint(ctx)
		if not SUPPRESS_CLASSES[child.ClassName] then
			local bucket = buckets[child.Name]
			if not bucket then
				bucket = {
					byClass = {},
					allowed = { items = {}, next = 1 },
					passThrough = { items = {}, next = 1 },
					boundaries = { items = {}, next = 1 },
				}
				buckets[child.Name] = bucket
			end
			local classList = bucket.byClass[child.ClassName]
			if not classList then
				classList = { items = {}, next = 1 }
				bucket.byClass[child.ClassName] = classList
			end
			classList.items[#classList.items + 1] = child
			if ALLOWED_CLASSES[child.ClassName] then
				bucket.allowed.items[#bucket.allowed.items + 1] = child
			else
				bucket.passThrough.items[#bucket.passThrough.items + 1] = child
			end
			if (ctx.useAvoidSyncIndex and ctx.avoidSyncBoundaries[child]) or hasAvoidSyncAttribute(child) then
				bucket.boundaries.items[#bucket.boundaries.items + 1] = child
			end
		end
	end

	local hasExactFragments = false
	for _, node in ipairs(nodes) do
		if type(node) == "table" and type(node.diskFragment) == "string" then
			hasExactFragments = true
			break
		end
	end
	if hasExactFragments and not ctx.useAvoidSyncIndex then
		ctx.useAvoidSyncIndex = true
		snapshotApplyState.indexAvoidSyncCarriers(parent, ctx)
	end
	local physical = if hasExactFragments then snapshotApplyState.buildPhysicalSiblingIndex(parent, ctx) else nil
	local assignments = {}
	local used = {}
	-- Exact physical identities are assigned as a batch before any fallback.
	-- This prevents a stale same-name disk node from consuming an AvoidSync
	-- reservation that a later exact sibling owns.
	for index, node in ipairs(nodes) do
		snapshotApplyState.checkpoint(ctx)
		local fragment = type(node) == "table" and node.diskFragment or nil
		local candidate = if type(fragment) == "string" and physical
			then physical.byFragment[string.lower(fragment)]
			else nil
		if candidate and (used[candidate] or ctx.claimedInstances[candidate]) then
			candidate = nil
		end
		if candidate then
			used[candidate] = true
		end
		assignments[index] = {
			node = node,
			candidate = candidate,
			hasExactIdentity = type(fragment) == "string",
		}
	end

	for _, assignment in ipairs(assignments) do
		if assignment.candidate then
			continue
		end
		local node = assignment.node
		local name = type(node) == "table" and node.name or nil
		local className = type(node) == "table" and node.class or nil
		local bucket = type(name) == "string" and buckets[name] or nil
		local candidate = nil
		if assignment.hasExactIdentity and bucket then
			-- If an ignored boundary's portable shape/class changed while it
			-- was hidden, it still consumes one same-logical-name disk node.
			-- Exact matches above always win before this safety fallback.
			candidate = snapshotApplyState.takeCandidate(bucket.boundaries, used, ctx.claimedInstances)
		elseif bucket then
			candidate = snapshotApplyState.takeCandidate(bucket.byClass[className], used, ctx.claimedInstances)
			if not candidate and className == "Folder" then
				-- A disk Folder may be the projection of a Studio-owned Model,
				-- Part, Tool, or other pass-through container.
				candidate = snapshotApplyState.takeCandidate(bucket.passThrough, used, ctx.claimedInstances)
			end
			if not candidate then
				-- Class changes are only legal for one of the four synced
				-- classes. Never replace a Studio-authoritative instance merely
				-- because it shares a name with a disk script.
				candidate = snapshotApplyState.takeCandidate(bucket.allowed, used, ctx.claimedInstances)
			end
		end
		assignment.candidate = candidate
	end
	return assignments
end

function snapshotApplyState.createInstance(parent, node, ctx)
	local ok, created = pcall(function()
		return Instance.new(node.class)
	end)
	if not ok then
		error("unknown class: " .. tostring(node.class), 0)
	end
	created.Name = node.name
	lastRemoteApplyClock = os.clock()
	snapshotApplyState.runInternalWrite(function()
		created.Parent = parent
	end, {
		instance = created,
		parent = true,
		structure = true,
		structureSubtree = true,
	})
	snapshotApplyState.checkpoint(ctx)
	return created
end

function snapshotApplyState.registerAppliedProjection(inst: Instance, node, ctx)
	-- The AvoidSync index is a snapshot of the service before this apply batch.
	-- Keep it current for Instances that this batch just materialized or matched,
	-- otherwise the next op cannot resolve an exact disk path through a newly
	-- created Folder (the stale index still considers that Folder unprojected).
	if ctx.avoidSyncBoundaries[inst] or ctx.avoidSyncCarriers[inst] then
		return
	end
	ctx.syncableInstances[inst] = true
	ctx.directoryInstances[inst] = if node.class == "Folder"
			or node.diskFragmentIsDir == true
			or (type(node.children) == "table" and #node.children > 0)
		then true
		else nil

	-- A newly applied descendant also makes every ordinary container above it
	-- part of the synchronized projection. This covers pass-through Models and
	-- other Studio-owned ancestors without rescanning the whole service per op.
	local ancestor = inst.Parent
	while ancestor and ancestor.Parent ~= game do
		if ctx.avoidSyncBoundaries[ancestor] then
			break
		end
		ctx.syncableInstances[ancestor] = true
		ctx.directoryInstances[ancestor] = true
		ancestor = ancestor.Parent
	end
end

function snapshotApplyState.prepareInstance(parent, assignment, opts, ctx)
	local node = assignment.node
	if type(node) ~= "table" or type(node.name) ~= "string" or not ALLOWED_CLASSES[node.class] then
		error(
			string.format(
				"invalid snapshot node %s/%s (%s)",
				pathOf(parent),
				tostring(type(node) == "table" and node.name or "?"),
				tostring(type(node) == "table" and node.class or "?")
			),
			0
		)
	end
	if node.children ~= nil and type(node.children) ~= "table" then
		error("invalid children for snapshot node " .. pathOf(parent) .. "/" .. node.name, 0)
	end

	local existing = assignment.candidate
	local existingBlocked = existing
		and (
			(ctx.useAvoidSyncIndex and ctx.avoidSyncBoundaries[existing])
			or hasAvoidSyncAttribute(existing)
			or (not ctx.useAvoidSyncIndex and isAvoidSyncBlocked(existing))
		)
	if existingBlocked then
		ctx.claimedInstances[existing] = true
		if type(node.diskFragment) == "string" then
			local parentDiskPath = snapshotApplyState.diskPathCache[parent]
			if parentDiskPath then
				local diskPath = table.clone(parentDiskPath)
				diskPath[#diskPath + 1] = node.diskFragment
				snapshotApplyState.diskPathCache[existing] = diskPath
			end
		end
		log("AvoidSync: skipping disk apply for " .. pathOf(existing))
		return existing, true
	end
	local inst = nil
	local applyNodeProperties = true
	if existing and existing.ClassName == node.class then
		inst = existing
		if ctx.avoidSyncCarriers[existing] then
			applyNodeProperties = false
		end
	elseif existing and node.class == "Folder" and not ALLOWED_CLASSES[existing.ClassName] then
		inst = existing
	elseif existing then
		-- Preserve children across a synced class change. In particular, never
		-- let choosing Disk indirectly destroy Studio-authoritative descendants.
		-- An AvoidSync descendant also protects the ancestor's identity because
		-- moving that descendant would change the ignored path.
		if ctx.avoidSyncCarriers[existing] then
			log(
				string.format(
					"AvoidSync: preserving carrier %s during class change %s → %s",
					pathOf(existing),
					existing.ClassName,
					tostring(node.class)
				)
			)
			inst = existing
			applyNodeProperties = false
		else
			local replacement = snapshotApplyState.createInstance(parent, node, ctx)
			for _, child in ipairs(existing:GetChildren()) do
				lastRemoteApplyClock = os.clock()
				snapshotApplyState.runInternalWrite(function()
					child.Parent = replacement
				end, {
					instance = child,
					parent = true,
					structure = true,
					structureSubtree = true,
				})
				snapshotApplyState.checkpoint(ctx)
			end
			lastRemoteApplyClock = os.clock()
			snapshotApplyState.runInternalWrite(function()
				existing:Destroy()
			end, {
				instance = existing,
				parent = true,
				structure = true,
				structureSubtree = true,
			})
			inst = replacement
		end
	else
		inst = snapshotApplyState.createInstance(parent, node, ctx)
	end

	ctx.claimedInstances[inst] = true
	snapshotApplyState.registerAppliedProjection(inst, node, ctx)
	if type(node.diskFragment) == "string" then
		local parentDiskPath = snapshotApplyState.diskPathCache[parent]
		if parentDiskPath then
			local diskPath = table.clone(parentDiskPath)
			diskPath[#diskPath + 1] = node.diskFragment
			snapshotApplyState.diskPathCache[inst] = diskPath
		end
	end
	if applyNodeProperties then
		applyProperties(inst, node.properties, opts)
	end
	return inst, false
end

-- Remove only the synchronized projection below unmatched children. Models,
-- Parts, UI, other Studio-owned containers, bare explorer Folders, and
-- AvoidSync carriers survive a strict Disk pull.
function snapshotApplyState.pruneUnkept(parent, keptInstances, ctx)
	local roots = {}
	local children = parent:GetChildren()
	for index = #children, 1, -1 do
		local child = children[index]
		if not keptInstances[child] then
			roots[#roots + 1] = child
		end
	end
	-- A synthetic parent keeps top-level siblings from being mistaken for one
	-- another's ancestor when a child frame is popped.
	local stack = {
		{
			synthetic = true,
			entered = true,
			children = roots,
			nextChild = 1,
			childProjection = false,
			protected = false,
		},
	}

	while #stack > 0 do
		snapshotApplyState.checkpoint(ctx)
		local frame = stack[#stack]
		if not frame.entered then
			local blocked = (ctx.useAvoidSyncIndex and ctx.avoidSyncBoundaries[frame.inst])
				or hasAvoidSyncAttribute(frame.inst)
				or (not ctx.useAvoidSyncIndex and isAvoidSyncBlocked(frame.inst))
			if blocked or SUPPRESS_CLASSES[frame.inst.ClassName] then
				local protected = blocked == true
				table.remove(stack)
				local ancestor = stack[#stack]
				if ancestor and protected then
					ancestor.protected = true
				end
			else
				frame.entered = true
				frame.children = frame.inst:GetChildren()
				frame.nextChild = 1
			end
		else
			local child = frame.children[frame.nextChild]
			if child then
				frame.nextChild += 1
				stack[#stack + 1] = {
					inst = child,
					entered = false,
					childProjection = false,
					protected = false,
				}
			else
				if frame.synthetic then
					table.remove(stack)
				else
					local inst = frame.inst
					local isScript = inst:IsA("LuaSourceContainer")
					local projectionRemoved = isScript or frame.childProjection
					local protected = frame.protected
					if isScript and not protected then
						local targetParent = inst.Parent
						if targetParent then
							-- Children left at this point are Studio-authoritative or
							-- projection-irrelevant. Move them out before deleting
							-- the stale synchronized script.
							for _, remaining in ipairs(inst:GetChildren()) do
								lastRemoteApplyClock = os.clock()
								snapshotApplyState.runInternalWrite(function()
									remaining.Parent = targetParent
								end, {
									instance = remaining,
									parent = true,
									structure = true,
									structureSubtree = true,
								})
								snapshotApplyState.checkpoint(ctx)
							end
							lastRemoteApplyClock = os.clock()
							snapshotApplyState.runInternalWrite(function()
								inst:Destroy()
							end, {
								instance = inst,
								parent = true,
								structure = true,
								structureSubtree = true,
							})
						end
					elseif
						inst.ClassName == "Folder"
						and frame.childProjection
						and not protected
						and #inst:GetChildren() == 0
					then
						lastRemoteApplyClock = os.clock()
						snapshotApplyState.runInternalWrite(function()
							inst:Destroy()
						end, {
							instance = inst,
							parent = true,
							structure = true,
							structureSubtree = true,
						})
					end

					table.remove(stack)
					local ancestor = stack[#stack]
					if ancestor then
						if projectionRemoved then
							ancestor.childProjection = true
						end
						if protected then
							ancestor.protected = true
						end
					end
				end
			end
		end
	end
end

function snapshotApplyState.runChildren(parent, nodes, opts, pruneRoot): { Instance }
	if isAvoidSyncBlocked(parent) then
		log("AvoidSync: skipping disk apply under " .. pathOf(parent))
		return {}
	end
	if type(nodes) ~= "table" then
		error("snapshot children must be an array", 0)
	end

	opts = opts or {}
	local ctx = opts._applyContext
	if not ctx then
		ctx = snapshotApplyState.newContext(opts)
		opts._applyContext = ctx
	end
	if opts.indexAvoidSyncCarriers then
		snapshotApplyState.indexAvoidSyncCarriers(parent, ctx)
	end
	local results = {}
	local stack = {
		{
			parent = parent,
			nodes = nodes,
			pruneParent = pruneRoot == true,
			prepared = false,
		},
	}

	while #stack > 0 do
		snapshotApplyState.checkpoint(ctx)
		local frame = stack[#stack]
		if not frame.prepared then
			frame.assignments = snapshotApplyState.buildAssignments(frame.parent, frame.nodes, ctx)
			frame.keptInstances = {}
			frame.nextAssignment = 1
			frame.prepared = true
		end

		local assignment = frame.assignments[frame.nextAssignment]
		if assignment then
			local assignmentIndex = frame.nextAssignment
			frame.nextAssignment += 1
			local inst, skipChildren = snapshotApplyState.prepareInstance(frame.parent, assignment, opts, ctx)
			frame.keptInstances[inst] = true
			if #stack == 1 then
				results[assignmentIndex] = inst
			end
			if not skipChildren then
				stack[#stack + 1] = {
					parent = inst,
					nodes = assignment.node.children or {},
					pruneParent = true,
					prepared = false,
				}
			end
		else
			if opts.strict and frame.pruneParent then
				snapshotApplyState.pruneUnkept(frame.parent, frame.keptInstances, ctx)
			end
			table.remove(stack)
		end
	end
	return results
end

local function applyNode(parent, node, opts)
	local results = snapshotApplyState.runChildren(parent, { node }, opts, false)
	return results[1]
end

local function applyClassChange(op, applyOpts, opContext)
	local newClass = tostring(op.class or "")
	if not SCRIPT_CLASSES[newClass] then
		log("class_change: unsupported class " .. newClass)
		return
	end
	local sourceDiskPath = if type(op.fromDiskPath) == "table"
		then op.fromDiskPath
		elseif type(op.diskPath) == "table" then op.diskPath
		else nil
	local target = if sourceDiskPath
		then snapshotApplyState.resolveDiskPath(sourceDiskPath, opContext)
		else resolveGeneratedPath(op.path, opContext)
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
	local destinationDiskPath = if type(op.toDiskPath) == "table" then op.toDiskPath else nil
	local resultingDiskPath = destinationDiskPath or sourceDiskPath
	local newParentPath = if destinationDiskPath then nil else parentPath(toPath)
	if destinationDiskPath or newParentPath then
		local resolvedParent = if destinationDiskPath
			then snapshotApplyState.resolveDiskParent(destinationDiskPath, opContext)
			else resolveGeneratedPath(newParentPath, opContext)
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
		applyProperties(target, op.properties, applyOpts)
		snapshotApplyState.clearDiskPathSubtree(target)
		if resultingDiskPath then
			snapshotApplyState.diskPathCache[target] = table.clone(resultingDiskPath)
		end
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
	applyProperties(replacement, op.properties, applyOpts)
	target:Destroy()
	snapshotApplyState.clearDiskPathSubtree(replacement)
	if resultingDiskPath then
		snapshotApplyState.diskPathCache[replacement] = table.clone(resultingDiskPath)
	end
end

local function applyOp(op, sharedApplyOpts)
	local kind = op.op or op.type
	local opContext = sharedApplyOpts and sharedApplyOpts._applyContext or nil
	if
		not opContext
		and (type(op.diskPath) == "table" or type(op.fromDiskPath) == "table" or type(op.toDiskPath) == "table")
	then
		opContext = snapshotApplyState.newContext({
			indexAvoidSyncCarriers = true,
		})
	end
	local generatedTarget = nil
	if type(op.targetPath) == "table" then
		generatedTarget = resolveGeneratedPath(op.targetPath, opContext)
		if generatedTarget and type(op.diskPath) == "table" then
			-- Selective snapshots can reference legacy/noncanonical physical
			-- fragments that are not derivable from current Studio shape alone.
			-- The daemon's exact generated target seeds that identity before
			-- buildAssignments consumes the node.
			snapshotApplyState.seedDiskPathAncestry(generatedTarget, op.diskPath)
		end
	end
	if sharedApplyOpts and sharedApplyOpts._applyContext then
		snapshotApplyState.checkpoint(sharedApplyOpts._applyContext)
	end
	if kind == "ensure" then
		if not rootSetAllowed(op) then
			return
		end
		local parent
		if generatedTarget and generatedTarget.Parent then
			parent = generatedTarget.Parent
		elseif type(op.diskPath) == "table" then
			parent = snapshotApplyState.resolveDiskParent(op.diskPath, opContext)
			if not parent and type(op.targetPath) == "table" then
				parent = resolveGeneratedPath(parentPath(op.targetPath), opContext)
			end
		elseif op.path and #op.path > 0 then
			parent = if op.pathMode == "generated"
				then resolveGeneratedPath(op.path, opContext)
				else resolvePath(op.path)
		else
			parent = game
		end
		if not parent then
			log("ensure: parent path not found")
			return
		end
		if isAvoidSyncBlocked(parent) then
			log("AvoidSync: ignoring ensure under " .. pathOf(parent))
			return
		end
		local node = op.node
		if type(node) ~= "table" or type(node.name) ~= "string" then
			log("ensure: invalid node")
			return
		end
		-- Selective initial sync may need a missing parent container to reach a
		-- chosen descendant. Existing ancestors are deliberately untouched,
		-- including class and Source, because the user did not stage them.
		local nodeOpts = snapshotApplyState.nodeOptions(sharedApplyOpts, false, false)
		if not nodeOpts._applyContext then
			nodeOpts._applyContext = snapshotApplyState.newContext(nodeOpts)
		end
		local assignment = snapshotApplyState.buildAssignments(parent, { node }, nodeOpts._applyContext)[1]
		if assignment and assignment.candidate then
			nodeOpts._applyContext.claimedInstances[assignment.candidate] = true
			return
		end
		applyNode(parent, node, nodeOpts)
	elseif kind == "set" or kind == "replace" then
		if not rootSetAllowed(op) then
			return
		end
		local parent
		if generatedTarget and generatedTarget.Parent then
			parent = generatedTarget.Parent
		elseif type(op.diskPath) == "table" then
			parent = snapshotApplyState.resolveDiskParent(op.diskPath, opContext)
			if not parent and type(op.targetPath) == "table" then
				parent = resolveGeneratedPath(parentPath(op.targetPath), opContext)
			end
		elseif op.path and #op.path > 0 then
			parent = if op.pathMode == "generated"
				then resolveGeneratedPath(op.path, opContext)
				else resolvePath(op.path)
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
		if type(op.diskPath) == "table" and type(op.node) == "table" and type(op.node.diskFragment) ~= "string" then
			op.node.diskFragment = op.diskPath[#op.diskPath]
			op.node.diskFragmentIsDir = op.diskFragmentIsDir
		end
		applyNode(
			parent,
			op.node,
			snapshotApplyState.nodeOptions(sharedApplyOpts, op.strict == true, op.forcePrune == true)
		)
	elseif kind == "delete" or kind == "remove" then
		local target = if type(op.diskPath) == "table"
			then snapshotApplyState.resolveDiskPath(op.diskPath, opContext)
			else resolveGeneratedPath(op.path, opContext)
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
		local target = if type(op.diskPath) == "table"
			then snapshotApplyState.resolveDiskPath(op.diskPath, opContext)
			else resolveGeneratedPath(op.path, opContext)
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
			local source = tostring(props.Source or "")
			if sharedApplyOpts and sharedApplyOpts.sourceWrites then
				queueScriptSourceWrite(sharedApplyOpts.sourceWrites, target, source, sharedApplyOpts.currentOp)
			elseif not writeScriptSource(target, source) then
				error("source update was not applied for " .. pathOf(target))
			end
		end
	elseif kind == "class_change" then
		applyClassChange(op, sharedApplyOpts, opContext)
	elseif kind == "rename" then
		if type(op.from) == "table" and type(op.to) == "table" then
			local target = if type(op.fromDiskPath) == "table"
				then snapshotApplyState.resolveDiskPath(op.fromDiskPath, opContext)
				else resolveGeneratedPath(op.from, opContext)
			if not target or target == game then
				return
			end
			if isAvoidSyncBlocked(target) then
				log("AvoidSync: ignoring rename for " .. pathOf(target))
				return
			end

			local toPath = op.to
			local newName = toPath[#toPath]
			local destinationDiskPath = if type(op.toDiskPath) == "table" then op.toDiskPath else nil
			local newParentPath = if destinationDiskPath then nil else parentPath(toPath)
			if destinationDiskPath or newParentPath then
				local newParent = if destinationDiskPath
					then snapshotApplyState.resolveDiskParent(destinationDiskPath, opContext)
					else resolveGeneratedPath(newParentPath, opContext)
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
			snapshotApplyState.clearDiskPathSubtree(target)
			if destinationDiskPath then
				snapshotApplyState.diskPathCache[target] = table.clone(destinationDiskPath)
			end
		else
			local target = if type(op.diskPath) == "table"
				then snapshotApplyState.resolveDiskPath(op.diskPath, opContext)
				else resolveGeneratedPath(op.path, opContext)
			if target and op.name and not isAvoidSyncBlocked(target) then
				target.Name = op.name
				snapshotApplyState.clearDiskPathSubtree(target)
			end
		end
	elseif kind == "move" then
		local target = if type(op.fromDiskPath) == "table"
			then snapshotApplyState.resolveDiskPath(op.fromDiskPath, opContext)
			else resolveGeneratedPath(op.from, opContext)
		local destination = type(op.to) == "table" and op.to or nil
		local destinationDiskPath = if type(op.toDiskPath) == "table" then op.toDiskPath else nil
		local destinationParent = if destinationDiskPath
			then snapshotApplyState.resolveDiskParent(destinationDiskPath, opContext)
			elseif destination then resolveGeneratedPath(parentPath(destination), opContext)
			else nil
		if
			target
			and destination
			and destinationParent
			and not isAvoidSyncBlocked(target)
			and not isAvoidSyncBlocked(destinationParent)
		then
			target.Parent = destinationParent
			local newName = destination[#destination]
			if newName and target.Name ~= newName then
				target.Name = newName
			end
			snapshotApplyState.clearDiskPathSubtree(target)
			if destinationDiskPath then
				snapshotApplyState.diskPathCache[target] = table.clone(destinationDiskPath)
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

-- Change-history bookkeeping for applyOps. Deliberately hung off
-- snapshotApplyState rather than declared as top-level locals: the main chunk
-- sits within a couple of registers of Luau's 200-local ceiling, and three more
-- top-level locals here made the whole plugin fail to compile at -O0.
snapshotApplyState.APPLY_RECORDING_ATTRIBUTE = "RoSyncActiveApplyRecording"
snapshotApplyState.applyRecordingRecovered = false

-- Studio recordings survive a plugin reload, so an apply interrupted mid-flight
-- used to leave ChangeHistoryService occupied for the rest of the session — and
-- because there is exactly one recording slot per plugin, every later apply then
-- failed forever. Cancel any identifier a previous run left behind. Runs once per
-- session: doing it again while a recording is legitimately open would cancel our
-- own in-flight apply.
function snapshotApplyState.recoverOrphanedApplyRecording()
	if snapshotApplyState.applyRecordingRecovered then
		return
	end
	snapshotApplyState.applyRecordingRecovered = true

	local stale = nil
	pcall(function()
		stale = plugin:GetAttribute(snapshotApplyState.APPLY_RECORDING_ATTRIBUTE)
	end)
	if type(stale) ~= "string" or stale == "" then
		return
	end
	log("applyOps: cancelling an orphaned change-history recording from a previous session")
	pcall(function()
		ChangeHistoryService:FinishRecording(stale, Enum.FinishRecordingOperation.Cancel)
	end)
	pcall(function()
		plugin:SetAttribute(snapshotApplyState.APPLY_RECORDING_ATTRIBUTE, nil)
	end)
end

local function applyOps(ops, shouldContinue): (boolean, { [any]: boolean }?)
	if not ops or #ops == 0 then
		return true
	end
	if shouldContinue and not shouldContinue() then
		return false
	end
	local label = "Ro Sync: " .. summarizeOps(ops)
	snapshotApplyState.recoverOrphanedApplyRecording()
	local beginOk, beginResult = pcall(function()
		return ChangeHistoryService:TryBeginRecording(label, label)
	end)
	local recordingId = if beginOk then beginResult else nil
	if recordingId == nil then
		-- Change history is unavailable: another plugin holds the single
		-- per-plugin recording slot, a solo playtest is running, or Studio
		-- refused for its own reasons. None of that is a reason to drop a
		-- filesystem edit on the floor. Fall back to a plain waypoint — which
		-- cannot fail — and apply anyway. Undo granularity degrades; the user's
		-- code still reaches Studio.
		local reason = if beginOk then "Studio refused the recording" else tostring(beginResult)
		log("applyOps: applying without a rollback-safe recording (" .. reason .. ")")
		setWaypoint(label)
	else
		pcall(function()
			plugin:SetAttribute(snapshotApplyState.APPLY_RECORDING_ATTRIBUTE, recordingId)
		end)
	end
	lastRemoteApplyClock = os.clock()
	local failedOps = {}
	local ok, err = pcall(function()
		local sourceWrites = {}
		local applyOpts = {
			sourceWrites = sourceWrites,
			shouldContinue = shouldContinue,
			claimedInstances = {},
			indexAvoidSyncCarriers = true,
		}
		applyOpts._applyContext = snapshotApplyState.newContext(applyOpts)
		for _, op in ipairs(ops) do
			if shouldContinue and not shouldContinue() then
				error("live operation apply cancelled", 0)
			end
			snapshotApplyState.checkpoint(applyOpts._applyContext)
			applyOpts.currentOp = op
			applyOp(op, applyOpts)
		end
		applyOpts.currentOp = nil
		if #sourceWrites > 0 then
			local failedJobs = {}
			local _, failedSources = flushScriptSourceWrites(sourceWrites, shouldContinue, failedJobs)
			if failedSources > 0 then
				-- A rejected Source is that op's problem, not the batch's. Failing the
				-- whole drain would roll back every sibling op and withhold every ack —
				-- and now that ops are coalesced, one bad script could strand an
				-- arbitrary number of good ones. Record the offenders so the caller
				-- withholds only their acks, and let the rest commit.
				for _, job in ipairs(failedJobs) do
					if job.op ~= nil then
						failedOps[job.op] = true
					end
				end
				log(string.format("applyOps: %d live Source write(s) rejected; committing the rest", failedSources))
			end
		end
		if shouldContinue and not shouldContinue() then
			error("live operation apply cancelled", 0)
		end
	end)
	lastRemoteApplyClock = os.clock()
	if ok and recordingId ~= nil then
		local finishOk, finishErr = pcall(function()
			snapshotApplyState.runInternalWrite(function()
				ChangeHistoryService:FinishRecording(recordingId, Enum.FinishRecordingOperation.Commit)
			end, {
				recording = true,
			})
		end)
		pcall(function()
			plugin:SetAttribute(snapshotApplyState.APPLY_RECORDING_ATTRIBUTE, nil)
		end)
		if not finishOk then
			ok = false
			err = "change-history commit failed: " .. tostring(finishErr)
		end
	elseif ok then
		setWaypoint(label)
	end
	if not ok then
		log("applyOps error: " .. tostring(err))
		if recordingId ~= nil then
			local cancelOk = pcall(function()
				snapshotApplyState.runInternalWrite(function()
					ChangeHistoryService:FinishRecording(recordingId, Enum.FinishRecordingOperation.Cancel)
				end, {
					recording = true,
				})
			end)
			pcall(function()
				plugin:SetAttribute(snapshotApplyState.APPLY_RECORDING_ATTRIBUTE, nil)
			end)
			if not cancelOk then
				pcall(function()
					ChangeHistoryService:Undo()
				end)
			end
		end
		for _, serviceName in ipairs(WATCHED_SERVICES) do
			local serviceOk, service = pcall(function()
				return game:GetService(serviceName)
			end)
			if serviceOk and service then
				snapshotApplyState.clearDiskPathSubtree(service)
			end
		end
		return false
	end
	return true, failedOps
end

local function sourceAckForAppliedOp(op)
	local kind = op.op or op.type
	local expected
	local path
	local diskPath
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
		diskPath = type(op.diskPath) == "table" and op.diskPath or nil
	elseif kind == "update" then
		if type(op.properties) ~= "table" or op.properties.Source == nil then
			return nil
		end
		expected = tostring(op.properties.Source or "")
		path = op.path
		diskPath = type(op.diskPath) == "table" and op.diskPath or nil
	elseif kind == "class_change" then
		if type(op.properties) ~= "table" or op.properties.Source == nil then
			return nil
		end
		expected = tostring(op.properties.Source or "")
		path = type(op.to) == "table" and op.to or op.path
		diskPath = if type(op.toDiskPath) == "table"
			then op.toDiskPath
			elseif type(op.diskPath) == "table" then op.diskPath
			else nil
	else
		return nil
	end

	if type(path) ~= "table" then
		return nil
	end
	local target = if diskPath
		then snapshotApplyState.resolveDiskPath(
			diskPath,
			snapshotApplyState.newContext({
				indexAvoidSyncCarriers = true,
			})
		)
		else resolveGeneratedPath(path)
	if not target or not target:IsA("LuaSourceContainer") then
		return nil
	end
	local actual = readScriptSource(target)
	if not sourcesMatchForApply(actual, expected) then
		return nil
	end
	local ack = {
		op = "update",
		path = path,
		properties = { Source = actual },
	}
	if diskPath then
		ack.diskPath = diskPath
	end
	return ack
end

local function applySnapshot(snap, shouldContinue): (boolean, string?)
	-- Apply a full snapshot payload returned by /snapshot.
	setWaypoint("Ro Sync: before snapshot")
	lastRemoteApplyClock = os.clock()
	local ok, err = pcall(function()
		local sourceWrites = {}
		local applyOpts = {
			strict = snap and snap.strict == true,
			forcePrune = snap and snap.forcePrune == true,
			sourceWrites = sourceWrites,
			shouldContinue = shouldContinue,
			claimedInstances = {},
			indexAvoidSyncCarriers = true,
		}
		applyOpts._applyContext = snapshotApplyState.newContext(applyOpts)
		if snap and snap.services then
			if type(snap.services) ~= "table" then
				error("snapshot services must be an array", 0)
			end
			for _, node in ipairs(snap.services) do
				if
					type(node) ~= "table"
					or type(node.name) ~= "string"
					or not WATCHED_SET[node.name]
					or type(node.children) ~= "table"
				then
					error("snapshot contains an invalid service node", 0)
				end
				local svc = game:GetService(node.name)
				snapshotApplyState.diskPathCache[svc] = { node.name }
				-- Services already exist and are outside the four-class
				-- whitelist, so reconcile their children as one identity-aware
				-- batch. This is what keeps duplicate names distinct.
				snapshotApplyState.runChildren(svc, node.children, applyOpts, true)
			end
		elseif snap and snap.ops then
			if type(snap.ops) ~= "table" then
				error("snapshot ops must be an array", 0)
			end
			for _, op in ipairs(snap.ops) do
				applyOp(op, applyOpts)
			end
		else
			error("snapshot payload has neither services nor ops", 0)
		end

		if #sourceWrites > 0 then
			local appliedSources, failedSources = flushScriptSourceWrites(sourceWrites, shouldContinue)
			log(
				string.format(
					"snapshot source writes: %d/%d applied with %d workers",
					appliedSources,
					#sourceWrites,
					math.min(SOURCE_WRITE_CONCURRENCY, #sourceWrites)
				)
			)
			if failedSources > 0 then
				error(string.format("%d snapshot source write(s) failed or were cancelled", failedSources), 0)
			end
		end
	end)
	lastRemoteApplyClock = os.clock()
	if not ok then
		log("applySnapshot error: " .. tostring(err))
		setWaypoint("Ro Sync: snapshot incomplete")
		return false, tostring(err)
	end
	setWaypoint("Ro Sync: snapshot applied")
	return true, nil
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
	-- Canonical project identity selected by the explicit Connect action.
	-- Recovery may follow that project to a new port/process, but must never
	-- attach to another local clone that happens to share GameId/PlaceId.
	pinnedProject = nil,
	-- Consecutive comparison restarts caused by Studio mutating mid-scan.
	-- Drives restart backoff; reset once a comparison reaches live sync.
	mutationRestarts = 0,
}
-- Forward-declared so wsLoop (below) can re-enter the initial-sync handshake
-- after any established transport gap. The real function assignment happens
-- further down.
local runInitialCompare: () -> ()
local disconnectHooks

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

-- Roblox grants each place ~500 HttpService requests per sliding minute and
-- rejects the excess with "Number of requests exceeded limit". A large
-- initial-sync stream can burn that budget mid-handshake; every recovery
-- restart then re-burns it from scratch, so the plugin starves itself forever
-- while the daemon is perfectly healthy. All plugin HTTP flows through
-- httpRequestTo, so pace it below the platform cap here: short bursts stay
-- instant, sustained streams are smoothed, and residual budget rejections
-- (e.g. another plugin sharing the budget) retry the same request in place —
-- a budget-rejected request never left Studio, so replaying it is safe.
local httpBudget = {
	windowSeconds = 60,
	burstRequests = 150, -- requests allowed at full speed per window
	maxRequests = 420, -- hard self-cap under Roblox's ~500/minute
	smoothingWait = 0.15, -- ≈6.6 req/s ≈ 400/min once the burst is spent
	stamps = {}, -- send-time circular queue: stamps[head..tail] are live
	head = 1,
	tail = 0,
}

local function httpRequestTo(url, method, body, internalJsonSafe, singleAttempt)
	local req = {
		Url = url,
		Method = method or "GET",
		Headers = { ["Content-Type"] = "application/json" },
	}
	if body ~= nil then
		-- Snapshot/bootstrap payloads are assembled exclusively from primitive
		-- schema fields by this plugin. Let those trusted internal callers skip
		-- sanitizeJson's full-tree copy: on a large service that otherwise
		-- doubles peak memory and adds another traversal before JSONEncode.
		-- Every general remote/audit caller keeps the sanitized default.
		local value = if internalJsonSafe == true then body else sanitizeJson(body)
		local encOk, enc = pcall(function()
			return HttpService:JSONEncode(value)
		end)
		if not encOk then
			return false, "JSONEncode failed: " .. tostring(enc)
		end
		req.Body = enc
	end

	local function liveStampCount(now)
		local stamps = httpBudget.stamps
		while httpBudget.head <= httpBudget.tail and now - stamps[httpBudget.head] >= httpBudget.windowSeconds do
			stamps[httpBudget.head] = nil
			httpBudget.head += 1
		end
		if httpBudget.head > httpBudget.tail then
			httpBudget.head, httpBudget.tail = 1, 0
		end
		return httpBudget.tail - httpBudget.head + 1
	end

	local function awaitSelfBudget()
		local smoothed = false
		while true do
			local now = os.clock()
			local live = liveStampCount(now)
			if live >= httpBudget.maxRequests then
				-- Sleep until the oldest live stamp ages out of the window,
				-- then re-check: another caller may have taken the freed slot.
				local wakeIn = httpBudget.windowSeconds - (now - httpBudget.stamps[httpBudget.head])
				task.wait(math.max(wakeIn, 0.1))
			elseif live >= httpBudget.burstRequests and not smoothed then
				smoothed = true
				task.wait(httpBudget.smoothingWait)
			else
				-- No yield between this check and the stamp in the caller, so
				-- concurrent callers cannot double-spend the same slot.
				return
			end
		end
	end

	local attempt = 0
	while true do
		attempt += 1
		awaitSelfBudget()
		httpBudget.tail += 1
		httpBudget.stamps[httpBudget.tail] = os.clock()
		local ok, resp = pcall(function()
			return HttpService:RequestAsync(req)
		end)
		if ok then
			return true, resp
		end
		local message = tostring(resp)
		local budgetRejected = message:lower():find("number of requests exceeded", 1, true) ~= nil
		if not budgetRejected or singleAttempt == true or attempt >= 12 then
			return false, message
		end
		-- The shared budget is exhausted (Studio internals or another plugin
		-- can spend it too). Refill is a sliding minute; waiting here keeps a
		-- multi-request handshake alive instead of failing and restarting it,
		-- which would re-spend every chunk already streamed.
		task.wait(5)
	end
end

local function httpRequest(path, method, body, internalJsonSafe)
	return httpRequestTo(baseUrl .. path, method, body, internalJsonSafe)
end

local function httpJson(path, method, body, internalJsonSafe, maxResponseBytes)
	local ok, resp = httpRequest(path, method, body, internalJsonSafe)
	if not ok then
		return false, resp
	end
	if not resp.Success then
		return false, string.format("HTTP %d %s", resp.StatusCode, resp.StatusMessage or "")
	end
	if resp.Body == nil or resp.Body == "" then
		return true, nil
	end
	if maxResponseBytes and #resp.Body > maxResponseBytes then
		return false, string.format("response exceeds %d bytes", maxResponseBytes)
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
local EDITOR_SOURCE_DEBOUNCE = 0.35
-- Group scale-related helpers/state behind one top-level register. This file is
-- intentionally close to Luau's 200-register chunk limit.
local scaleState = {
	avoidSyncRoots = {},
	avoidSyncPathsDirty = false,
	avoidSyncPushPending = false,
	avoidSyncPushInFlight = false,
	avoidSyncRevision = 0,
	avoidSyncTrackingPaused = false,
	avoidSyncPushDebounce = 0.25,
	hookYieldEvery = 750,
	studioScanYieldEvery = 500,
	structureChunkRecords = 512,
	compareHashChunkRecords = 512,
	sourcePartChunkRecords = 64,
	deleteChunkRecords = 512,
	sourceChunkBytes = 64 * 1024,
	maxStreamRequestBytes = PROTOCOL_STREAM_MAX_BYTES,
	maxStreamNodes = 1_000_000,
	maxStreamDepth = 256,
	maxScriptSourceBytes = 32 * 1024 * 1024,
	maxRecoveryReceiptStringBytes = 64 * 1024,
	-- Detached pull staging is intentionally bounded independently from the
	-- per-script wire limit. Atomic all-service commit retains every staged
	-- service until the authenticated terminal response arrives.
	maxStagedSourceBytesPerService = 64 * 1024 * 1024,
	maxStagedSourceBytesTotal = 128 * 1024 * 1024,
	emptyConnectionBucket = table.freeze({}),
	itemChangedAvailable = false,
	hookGeneration = 0,
	liveResyncPending = false,
	trackedInstanceCount = 0,
	trackedConnectionCount = 0,
	lastKnownParent = {},
}
local flushPush

scaleState.preflightLiveOp = function(op, shouldContinue): (string?, string?)
	-- Reject an oversized/cyclic value tree before sanitizeJson duplicates it.
	-- This is deliberately iterative so a huge pass-through subtree cannot
	-- overflow the Luau stack or monopolize Studio's UI thread.
	local estimated = 32
	local visited = 0
	local seen = {}
	local stack = { { value = op, depth = 0 } }
	while #stack > 0 do
		local entry = table.remove(stack)
		local value = entry.value
		local valueType = typeof(value)
		if valueType == "table" then
			if seen[value] then
				return nil, "live operation contains a cyclic/shared table"
			end
			if entry.depth > 128 then
				return nil, "live operation exceeds the JSON depth limit"
			end
			seen[value] = true
			estimated += 2
			for key, child in pairs(value) do
				estimated += if type(key) == "string" then #key + 4 else 2
				stack[#stack + 1] = {
					value = child,
					depth = entry.depth + 1,
				}
			end
		elseif valueType == "string" then
			estimated += #value + 2
		elseif valueType == "number" then
			estimated += 24
		elseif valueType == "boolean" then
			estimated += 5
		elseif valueType ~= "nil" then
			return nil, "live operation contains a non-JSON value"
		end
		if estimated > LIVE_PUSH_MAX_BYTES then
			return nil, "live operation exceeds the 512 KiB frame limit"
		end
		visited += 1
		if visited % scaleState.studioScanYieldEvery == 0 then
			task.wait()
			if shouldContinue and not shouldContinue() then
				return nil, "cancelled"
			end
		end
	end

	local clean = sanitizeJson(op)
	local encodedOk, encoded = pcall(function()
		return HttpService:JSONEncode(clean)
	end)
	if not encodedOk then
		return nil, "JSONEncode failed: " .. tostring(encoded)
	end
	-- Account for the fixed envelope plus a comma in a multi-op batch.
	if #encoded + 32 > LIVE_PUSH_MAX_BYTES then
		return nil, "live operation exceeds the 512 KiB frame limit"
	end
	return encoded, nil
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
	local generation = wsGeneration
	local function shouldContinue()
		return reconnectState.desired
			and connected
			and generation == wsGeneration
			and ws == client
			and not scaleState.liveResyncPending
	end

	-- Preflight every operation before sending the first frame. If one Source
	-- or subtree cannot fit, no prefix is sent; the bounded bootstrap stream
	-- becomes the sole source of truth.
	for index, op in ipairs(ops) do
		local preflightEncoded, encodeErr = scaleState.preflightLiveOp(op, shouldContinue)
		if not preflightEncoded then
			if encodeErr ~= "cancelled" and shouldContinue() then
				scaleState.requestControlledLiveResync(
					string.format("live operation %d requires streamed resync: %s", index, tostring(encodeErr))
				)
			end
			return
		end
	end
	if not shouldContinue() then
		return
	end

	local prefix = '{"type":"push","ops":['
	local suffix = "]}"
	local batch = {}
	local batchBytes = #prefix + #suffix
	for index, op in ipairs(ops) do
		-- Re-encode only the current bounded frame after the discard-only
		-- preflight pass. Retaining every encoded operation here would turn a
		-- large burst into another whole-project-sized memory spike.
		local encoded, encodeErr = scaleState.preflightLiveOp(op, shouldContinue)
		if not encoded then
			if encodeErr ~= "cancelled" and shouldContinue() then
				scaleState.requestControlledLiveResync(
					string.format("live operation %d changed during bounded encoding: %s", index, tostring(encodeErr))
				)
			end
			return
		end
		local separatorBytes = if #batch == 0 then 0 else 1
		if #batch > 0 and batchBytes + separatorBytes + #encoded > LIVE_PUSH_MAX_BYTES then
			local frame = prefix .. table.concat(batch, ",") .. suffix
			local sendOk, sendErr = pcall(function()
				client:Send(frame)
			end)
			if not sendOk then
				if shouldContinue() then
					scaleState.requestControlledLiveResync("bounded live push failed: " .. tostring(sendErr))
				end
				return
			end
			batch = {}
			batchBytes = #prefix + #suffix
			if not shouldContinue() then
				return
			end
		end
		batch[#batch + 1] = encoded
		batchBytes += (if #batch == 1 then 0 else 1) + #encoded
		if index % scaleState.structureChunkRecords == 0 then
			task.wait()
			if not shouldContinue() then
				return
			end
		end
	end
	if #batch > 0 then
		local frame = prefix .. table.concat(batch, ",") .. suffix
		local sendOk, sendErr = pcall(function()
			client:Send(frame)
		end)
		if not sendOk and shouldContinue() then
			scaleState.requestControlledLiveResync("bounded live push failed: " .. tostring(sendErr))
		end
	end
end

local function queueOp(op, bypassClockEchoGuard: boolean?)
	if not bypassClockEchoGuard and isEchoing() then
		return
	end
	-- Validation and JSON encoding happen during the bounded two-pass flush.
	-- Repeating the full tree walk here made every live Studio edit pay for
	-- three sanitize/encode passes before it reached the daemon.
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
	local op = {
		op = "update",
		path = pathFromRoot(inst),
		properties = { Source = source },
	}
	-- A script with projected children is represented by a directory. Source
	-- belongs to its init marker file, never to that directory itself.
	local diskPath = snapshotApplyState.sourceDiskPathForInstance(inst)
	if diskPath then
		op.diskPath = diskPath
	end
	-- Source echo suppression is per-script and exact-value based. Never let
	-- an unrelated long-running remote apply's global structure clock discard
	-- a genuine editor commit.
	queueOp(op, true)
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
	local source = tryReadScriptSource(scriptInst)
	return source
end

local function queueEditorSourceChanged(scriptInst: Instance, source: string)
	if sourceSyncState.consumeExpected(scriptInst, source) then
		return
	end
	local sourceHash = sha256Hex(source)
	local pending = sourceSyncState.pending[scriptInst]
	if pending and pending.sourceHash == sourceHash then
		return
	end
	local serial = pending and (pending.serial + 1) or 1
	sourceSyncState.pending[scriptInst] = {
		sourceHash = sourceHash,
		serial = serial,
		generation = sourceSyncState.generation[scriptInst] or 0,
	}
	task.delay(EDITOR_SOURCE_DEBOUNCE, function()
		local latest = sourceSyncState.pending[scriptInst]
		if
			not latest
			or latest.serial ~= serial
			or latest.generation ~= (sourceSyncState.generation[scriptInst] or 0)
		then
			return
		end
		if scriptInst.Parent == nil then
			sourceSyncState.pending[scriptInst] = nil
			return
		end
		if not isUnderWatched(scriptInst) then
			sourceSyncState.pending[scriptInst] = nil
			return
		end
		if isGeneratedDiskRoot(scriptInst) then
			sourceSyncState.pending[scriptInst] = nil
			return
		end
		if isAvoidSyncBlocked(scriptInst) then
			sourceSyncState.pending[scriptInst] = nil
			return
		end
		if not scriptInst:IsA("LuaSourceContainer") then
			sourceSyncState.pending[scriptInst] = nil
			return
		end
		-- TextDocumentDidChange may fire many times before this callback. Commit
		-- the editor's current text, not the stale text captured by the first
		-- event in the debounce window.
		local currentSource, sourceErr = tryReadScriptSource(scriptInst)
		if currentSource == nil then
			sourceSyncState.pending[scriptInst] = nil
			log("editor Source read failed: " .. tostring(sourceErr))
			if connected then
				scaleState.requestControlledLiveResync("Studio editor Source became unreadable")
			end
			return
		end
		sourceSyncState.pending[scriptInst] = nil
		if sourceSyncState.consumeExpected(scriptInst, currentSource) then
			return
		end
		queueScriptUpdateOp(scriptInst, currentSource)
	end)
end

--------------------------------------------------------------------------------
-- AvoidSync path push (Studio -> daemon)
--------------------------------------------------------------------------------
-- The daemon only consumed full tree snapshots to recover AvoidSync prefixes.
-- Track those roots directly so a source edit or ordinary Explorer mutation
-- does not serialize every Instance in the place.

scaleState.avoidSyncPathAffectedBy = function(inst: Instance): boolean
	for root in pairs(scaleState.avoidSyncRoots) do
		local current = root
		while current and current ~= game do
			if current == inst then
				return true
			end
			current = current.Parent
		end
	end
	return false
end

scaleState.collectAvoidSyncPaths = function(): { { string } }
	local keyed = {}
	for root in pairs(scaleState.avoidSyncRoots) do
		if hasAvoidSyncAttribute(root) then
			local path = pathFromRoot(root)
			if WATCHED_SET[path[1]] then
				keyed[#keyed + 1] = {
					key = table.concat(path, "\0"),
					path = path,
				}
			else
				scaleState.avoidSyncRoots[root] = nil
			end
		else
			scaleState.avoidSyncRoots[root] = nil
		end
	end
	table.sort(keyed, function(left, right)
		return left.key < right.key
	end)
	local paths = {}
	for _, entry in ipairs(keyed) do
		paths[#paths + 1] = entry.path
	end
	return paths
end

scaleState.pushAvoidSyncPaths = function()
	if not scaleState.avoidSyncPathsDirty or not connected or scaleState.avoidSyncPushInFlight then
		return
	end
	scaleState.avoidSyncPushInFlight = true
	local revision = scaleState.avoidSyncRevision
	local paths = scaleState.collectAvoidSyncPaths()
	local ok, err = httpJson("/tree", "POST", {
		version = 2,
		avoidSyncPaths = paths,
	})
	scaleState.avoidSyncPushInFlight = false
	if ok then
		if scaleState.avoidSyncRevision == revision then
			scaleState.avoidSyncPathsDirty = false
		end
		log(string.format("AvoidSync index: %d root path(s)", #paths))
	else
		log("AvoidSync path push err: " .. tostring(err))
	end
	if scaleState.avoidSyncPathsDirty and connected and not scaleState.avoidSyncPushPending then
		scaleState.avoidSyncPushPending = true
		task.delay(scaleState.avoidSyncPushDebounce, function()
			scaleState.avoidSyncPushPending = false
			scaleState.pushAvoidSyncPaths()
		end)
	end
end

scaleState.markAvoidSyncPathsDirty = function()
	if scaleState.avoidSyncTrackingPaused then
		return
	end
	scaleState.avoidSyncRevision += 1
	scaleState.avoidSyncPathsDirty = true
	if scaleState.avoidSyncPushPending or scaleState.avoidSyncPushInFlight then
		return
	end
	scaleState.avoidSyncPushPending = true
	task.delay(scaleState.avoidSyncPushDebounce, function()
		scaleState.avoidSyncPushPending = false
		scaleState.pushAvoidSyncPaths()
	end)
end

--------------------------------------------------------------------------------
-- Hooks
--------------------------------------------------------------------------------

-- Per-instance property connections plus service-level descendant connections.
-- ChildAdded/ChildRemoved on every Instance cost two extra signals per object;
-- service DescendantAdded/DescendantRemoving preserve the same coverage with a
-- fixed number of structural signals. A single DataModel.ItemChanged signal
-- covers property edits when available; scoped per-instance signals are kept
-- only as a compatibility fallback.
local connections: { [Instance]: any } = {}
local serviceConnections: { RBXScriptConnection } = {}
local activeConnectionSet: { [Instance]: true } = {}
local avoidAttributeConnectionSet: { [Instance]: true } = {}
local baseConnectionSet: { [Instance]: true } = {}
local lastAvoidSyncValue = setmetatable({} :: { [Instance]: boolean }, { __mode = "k" })
-- Name at the time we first connected the instance. When the Name signal fires,
-- the instance already holds the new name and pathFromRoot would
-- reconstruct the new path — useless for telling the daemon *which* file to
-- rename. Tracking the prior name here lets us emit a `rename` op with the
-- correct old path + new name.
local lastKnownName: { [Instance]: string } = {}
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

local function recordMaterializedAncestors(inst)
	local current = inst
	while current and current.Parent and current.Parent ~= game do
		if SUPPRESS_CLASSES[current.ClassName] or isAvoidSyncBlocked(current) then
			return
		end
		if current.ClassName == "Folder" or not ALLOWED_CLASSES[current.ClassName] then
			materializedContainers[current] = true
			connectInstance(current, false, true)
		end
		current = current.Parent
	end
end

scaleState.pathForParent = function(parent: Instance?, name: string): { string }?
	if not parent then
		return nil
	end
	local path = pathFromRoot(parent)
	if #path == 0 or not WATCHED_SET[path[1]] then
		return nil
	end
	path[#path + 1] = name
	return path
end

scaleState.decorateSetIdentity = function(inst: Instance, node, op, diskPath)
	local exactPath = diskPath or snapshotApplyState.diskPathForInstance(inst)
	if not exactPath or type(node) ~= "table" then
		return
	end
	node.diskFragment = exactPath[#exactPath]
	node.diskFragmentIsDir = node.class == "Folder" or (type(node.children) == "table" and #node.children > 0)
	op.diskPath = exactPath
end

scaleState.queueRepresentationSet = function(inst: Instance, node, fromDiskPath, toDiskPath)
	if not inst.Parent or type(node) ~= "table" or not fromDiskPath or not toDiskPath then
		return
	end
	local op = {
		op = "set",
		path = pathFromRoot(inst.Parent),
		fromDiskPath = fromDiskPath,
		diskPath = toDiskPath,
		node = node,
	}
	node.diskFragment = toDiskPath[#toDiskPath]
	node.diskFragmentIsDir = node.class == "Folder" or (type(node.children) == "table" and #node.children > 0)
	snapshotApplyState.diskPathCache[inst] = table.clone(toDiskPath)
	queueOp(op)
end

scaleState.clearDiskPathCache = function(root: Instance)
	snapshotApplyState.clearDiskPathSubtree(root)
end

scaleState.nearestScriptAncestor = function(parent: Instance?): Instance?
	local current = parent
	while current and current ~= game do
		if current:IsA("LuaSourceContainer") then
			return current
		end
		current = current.Parent
	end
	return nil
end

scaleState.syncExcludedAt = function(inst: Instance, parent: Instance?, path: { string }?): boolean
	if SUPPRESS_CLASSES[inst.ClassName] or hasAvoidSyncAttribute(inst) then
		return true
	end
	local current = parent
	while current and current ~= game do
		if SUPPRESS_CLASSES[current.ClassName] or hasAvoidSyncAttribute(current) then
			return true
		end
		current = current.Parent
	end
	return pathStartsWith(path or {}, wallyRootSegments)
end

scaleState.serializeLiveInstance = function(inst: Instance)
	local generation = wsGeneration
	local function shouldContinue()
		return reconnectState.desired
			and (connected or busy)
			and generation == wsGeneration
			and not scaleState.liveResyncPending
	end
	local node, serializeErr = serializeInstance(inst, shouldContinue)
	if not node and serializeErr and serializeErr ~= "cancelled" and shouldContinue() then
		if serializeErr == "oversized" then
			scaleState.requestControlledLiveResync("large Studio subtree requires the bounded bootstrap stream")
		else
			scaleState.requestControlledLiveResync("Studio subtree Source read failed: " .. tostring(serializeErr))
		end
	end
	return node
end

onDescendantAdded = function(inst)
	if isEchoing() then
		return
	end
	if not isUnderWatched(inst) then
		return
	end
	if isAvoidSyncBlocked(inst) then
		return
	end
	if isGeneratedDiskRoot(inst) then
		return
	end
	if SUPPRESS_CLASSES[inst.ClassName] then
		return
	end
	-- Emit a `set` for:
	--   (a) ALLOWED classes directly, or
	--   (b) non-ALLOWED classes that already have ALLOWED descendants (e.g. a
	--       Part being reparented into Workspace with a Script already inside
	--       it — we need the pass-through directory to materialize at once).
	-- A bare non-ALLOWED with no syncable descendants is live-tree-only.
	local node = scaleState.serializeLiveInstance(inst)
	if not node then
		return
	end
	local representationParent = inst.Parent
	while
		representationParent
		and representationParent ~= game
		and not representationParent:IsA("LuaSourceContainer")
	do
		representationParent = representationParent.Parent
	end
	if representationParent and representationParent:IsA("LuaSourceContainer") then
		local parentNode = scaleState.serializeLiveInstance(representationParent)
		if parentNode and type(parentNode.children) == "table" and #parentNode.children == 1 then
			local oldDiskPath = snapshotApplyState.diskPathCache[representationParent]
				or snapshotApplyState.diskPathForInstanceShape(representationParent, false)
			local newDiskPath = snapshotApplyState.diskPathForInstanceShape(representationParent, true)
			if oldDiskPath and newDiskPath and table.concat(oldDiskPath, "\0") ~= table.concat(newDiskPath, "\0") then
				-- A leaf script is a file, so its first projected child cannot
				-- be written below it until the daemon atomically migrates the
				-- parent to a script-with-children directory.
				scaleState.queueRepresentationSet(representationParent, parentNode, oldDiskPath, newDiskPath)
			end
		end
	end
	-- When a script is added directly inside an existing pass-through Part,
	-- this fires with `inst = Script` whose parent is the Part. The Part's
	-- directory on disk is created implicitly when the daemon writes the
	-- script file — but we still mark the Part as materialized so a later
	-- Removing on the Part emits a cleanup delete.
	recordMaterializedAncestors(inst.Parent)
	snapshotApplyState.diskPathCache[inst] = nil
	local setOp = {
		op = "set",
		path = pathFromRoot(inst.Parent),
		node = node,
	}
	scaleState.decorateSetIdentity(inst, node, setOp)
	queueOp(setOp)
end

onDescendantRemoving = function(inst)
	if isEchoing() then
		return
	end
	-- DescendantRemoving fires before the instance is unparented, so the old
	-- path is available on demand and does not need to be cached for every node.
	local path = pathFromRoot(inst)
	if not path or not WATCHED_SET[path[1]] then
		return
	end
	if isAvoidSyncBlocked(inst) then
		return
	end
	if isGeneratedDiskRoot(inst) then
		return
	end
	-- `Removing` fires top-down as children are re-parented to nil: by the
	-- time it's the pass-through container's turn, its syncable children have
	-- already been detached and `hasSyncableDescendant` returns false. Use
	-- the `materializedContainers` record to recover: if we ever emitted this
	-- instance as a directory on disk, emit a cleanup delete regardless.
	local syncable = materializedContainers[inst] == true or isSyncable(inst)
	materializedContainers[inst] = nil
	if not syncable then
		return
	end
	local diskPath = snapshotApplyState.diskPathForInstance(inst)
	log(string.format("delete: %s", table.concat(path, "/")))
	local deleteOp = {
		op = "delete",
		path = path,
	}
	if diskPath then
		deleteOp.diskPath = diskPath
	end
	queueOp(deleteOp)

	local representationParent = inst.Parent
	while
		representationParent
		and representationParent ~= game
		and not representationParent:IsA("LuaSourceContainer")
	do
		representationParent = representationParent.Parent
	end
	if representationParent and representationParent:IsA("LuaSourceContainer") then
		local parentNode = scaleState.serializeLiveInstance(representationParent)
		if parentNode and type(parentNode.children) == "table" and #parentNode.children == 1 then
			local oldParentDiskPath = snapshotApplyState.diskPathCache[representationParent]
				or snapshotApplyState.diskPathForInstanceShape(representationParent, true)
			task.defer(function()
				if
					isEchoing()
					or not representationParent.Parent
					or not isUnderWatched(representationParent)
					or isAvoidSyncBlocked(representationParent)
					or isGeneratedDiskRoot(representationParent)
				then
					return
				end
				local leafNode = scaleState.serializeLiveInstance(representationParent)
				if not leafNode or type(leafNode.children) ~= "table" or #leafNode.children ~= 0 then
					return
				end
				local leafDiskPath = snapshotApplyState.diskPathForInstanceShape(representationParent, false)
				if
					oldParentDiskPath
					and leafDiskPath
					and table.concat(oldParentDiskPath, "\0") ~= table.concat(leafDiskPath, "\0")
				then
					-- Delete the last child first, then atomically collapse the
					-- now-empty directory back to the leaf script file.
					scaleState.queueRepresentationSet(representationParent, leafNode, oldParentDiskPath, leafDiskPath)
				end
			end)
		end
	end
end

scaleState.onParentChanged = function(inst: Instance)
	local oldParent = scaleState.lastKnownParent[inst]
	local newParent = inst.Parent
	local oldDiskPath = snapshotApplyState.diskPathCache[inst]
	scaleState.lastKnownParent[inst] = newParent
	if oldParent == newParent then
		return
	end
	if scaleState.avoidSyncPathAffectedBy(inst) then
		scaleState.markAvoidSyncPathsDirty()
	end
	if isEchoing() then
		return
	end

	local name = lastKnownName[inst] or inst.Name
	local oldPath = scaleState.pathForParent(oldParent, name)
	local newPath = if newParent then pathFromRoot(inst) else nil
	-- Service-level descendant signals cover entering/leaving a service, but
	-- Roblox does not fire them for a reparent that stays under the same
	-- service. Handle exactly that missing case here.
	if not oldPath or not newPath or not WATCHED_SET[newPath[1]] or oldPath[1] ~= newPath[1] then
		return
	end

	local oldExcluded = scaleState.syncExcludedAt(inst, oldParent, oldPath)
	local newExcluded = scaleState.syncExcludedAt(inst, newParent, newPath)
	if oldExcluded and newExcluded then
		return
	end
	if oldExcluded then
		onDescendantAdded(inst)
		connectTree(inst)
		return
	end
	if newExcluded then
		if inst:IsA("LuaSourceContainer") or materializedContainers[inst] == true then
			log(string.format("delete after reparent into excluded subtree: %s", table.concat(oldPath, "/")))
			local deleteOp = {
				op = "delete",
				path = oldPath,
			}
			if oldDiskPath then
				deleteOp.diskPath = oldDiskPath
			end
			queueOp(deleteOp)
		end
		return
	end
	if materializedContainers[inst] ~= true and not isSyncable(inst) then
		return
	end

	local oldRepresentationParent = scaleState.nearestScriptAncestor(oldParent)
	local newRepresentationParent = scaleState.nearestScriptAncestor(newParent)
	local destinationMigrated = false
	if newRepresentationParent and newRepresentationParent ~= oldRepresentationParent then
		local parentNode = scaleState.serializeLiveInstance(newRepresentationParent)
		if parentNode and type(parentNode.children) == "table" and #parentNode.children == 1 then
			local oldParentDiskPath = snapshotApplyState.diskPathCache[newRepresentationParent]
				or snapshotApplyState.diskPathForInstanceShape(newRepresentationParent, false)
			local newParentDiskPath = snapshotApplyState.diskPathForInstanceShape(newRepresentationParent, true)
			if
				oldParentDiskPath
				and newParentDiskPath
				and table.concat(oldParentDiskPath, "\0") ~= table.concat(newParentDiskPath, "\0")
			then
				scaleState.queueRepresentationSet(
					newRepresentationParent,
					parentNode,
					oldParentDiskPath,
					newParentDiskPath
				)
				destinationMigrated = true
			end
		end
	end

	recordMaterializedAncestors(newParent)
	scaleState.clearDiskPathCache(inst)
	local newDiskPath = snapshotApplyState.diskPathForInstance(inst)
	if destinationMigrated then
		-- The destination migration serialized the moved subtree while making
		-- the leaf parent traversable. Remove the old exact copy instead of
		-- asking the daemon to move onto the newly materialized target.
		local deleteOp = {
			op = "delete",
			path = oldPath,
		}
		if oldDiskPath then
			deleteOp.diskPath = oldDiskPath
		end
		queueOp(deleteOp)
	else
		log(string.format("move: %s → %s", table.concat(oldPath, "/"), table.concat(newPath, "/")))
		local moveOp = {
			op = "move",
			from = oldPath,
			to = newPath,
		}
		if oldDiskPath and newDiskPath then
			moveOp.fromDiskPath = oldDiskPath
			moveOp.toDiskPath = newDiskPath
		end
		queueOp(moveOp)
	end

	if oldRepresentationParent and oldRepresentationParent ~= newRepresentationParent then
		local leafNode = scaleState.serializeLiveInstance(oldRepresentationParent)
		if leafNode and type(leafNode.children) == "table" and #leafNode.children == 0 then
			local oldParentDiskPath = snapshotApplyState.diskPathCache[oldRepresentationParent]
				or snapshotApplyState.diskPathForInstanceShape(oldRepresentationParent, true)
			local leafDiskPath = snapshotApplyState.diskPathForInstanceShape(oldRepresentationParent, false)
			if
				oldParentDiskPath
				and leafDiskPath
				and table.concat(oldParentDiskPath, "\0") ~= table.concat(leafDiskPath, "\0")
			then
				scaleState.queueRepresentationSet(oldRepresentationParent, leafNode, oldParentDiskPath, leafDiskPath)
			end
		end
	end
end

onChanged = function(inst, prop)
	local previousName = nil
	if prop == "Name" then
		previousName = lastKnownName[inst] or inst.Name
		-- Echoed disk renames still advance the cache; otherwise the next
		-- Studio rename would target a stale filesystem path.
		lastKnownName[inst] = inst.Name
		if scaleState.avoidSyncPathAffectedBy(inst) then
			scaleState.markAvoidSyncPathsDirty()
		end
	end
	local sourceChange = prop == "Source" and inst:IsA("LuaSourceContainer")
	if not sourceChange and isEchoing() then
		return
	end
	if not isUnderWatched(inst) then
		return
	end
	if isGeneratedDiskRoot(inst) then
		return
	end
	if isAvoidSyncBlocked(inst) then
		return
	end
	if sourceChange then
		local source, sourceErr = tryReadScriptSource(inst)
		if source == nil then
			log("Source change could not read the Studio editor buffer: " .. tostring(sourceErr))
			if connected then
				scaleState.requestControlledLiveResync("Studio Source change could not be read")
			end
			return
		end
		queueEditorSourceChanged(inst, source)
		return
	end
	if prop == "Name" then
		-- Non-syncable instances don't exist on disk, so there's no file to
		-- rename.
		if materializedContainers[inst] ~= true and not isSyncable(inst) then
			return
		end
		-- Dedicated rename op: daemon needs the OLD path to locate the file
		-- it has to rename. pathFromRoot(inst) already reflects the new name
		-- because Changed fires after the assignment, so we reconstruct the
		-- old path from the cached last-known name.
		local oldName = previousName or inst.Name
		local oldDiskPath = snapshotApplyState.diskPathCache[inst]
		local parentPath = inst.Parent and pathFromRoot(inst.Parent) or {}
		local oldPath = {}
		for i, segment in ipairs(parentPath) do
			oldPath[i] = segment
		end
		oldPath[#oldPath + 1] = oldName
		scaleState.clearDiskPathCache(inst)
		local newDiskPath = snapshotApplyState.diskPathForInstance(inst)
		log(string.format("rename: %s → %s (path depth %d)", oldName, inst.Name, #oldPath))
		local renameOp = {
			op = "rename",
			path = oldPath,
			name = inst.Name,
		}
		if oldDiskPath and newDiskPath then
			renameOp.diskPath = oldDiskPath
			renameOp.fromDiskPath = oldDiskPath
			renameOp.toDiskPath = newDiskPath
		end
		queueOp(renameOp)
		return
	end
	-- Every other property is Studio-authoritative. No op, no tree push —
	-- live-tree shape depends on class+name+children, none of which change
	-- on a plain property edit.
end

onAvoidSyncChanged = function(inst)
	local current = hasAvoidSyncAttribute(inst)
	lastAvoidSyncValue[inst] = current
	if isEchoing() then
		return
	end
	if not isUnderWatched(inst) then
		return
	end
	if current then
		scaleState.avoidSyncRoots[inst] = true
		scaleState.markAvoidSyncPathsDirty()
		log("AvoidSync: ignoring " .. pathOf(inst))
		return
	end
	scaleState.avoidSyncRoots[inst] = nil
	scaleState.markAvoidSyncPathsDirty()
	connectTree(inst)
	if inst.Parent then
		local node = scaleState.serializeLiveInstance(inst)
		if not node then
			return
		end
		scaleState.clearDiskPathCache(inst)
		local setOp = {
			op = "set",
			path = pathFromRoot(inst.Parent),
			node = node,
		}
		scaleState.decorateSetIdentity(inst, node, setOp)
		queueOp(setOp)
	end
end

disconnectInstance = function(inst)
	sourceSyncState.pending[inst] = nil
	sourceSyncState.expected[inst] = nil
	sourceSyncState.generation[inst] = nil
	local bucket = connections[inst]
	if not bucket then
		-- Clear the key even when the bucket is missing or falsy. disconnectHooks
		-- used to drain with `while next(connections)`, so any key that survived
		-- this early return spun that loop forever and killed the plugin with
		-- "script exhaustion time exceeded" during teardown.
		connections[inst] = nil
		return
	end
	for _, c in ipairs(bucket) do
		-- pcall the method directly rather than wrapping it in a closure: this
		-- runs once per connection across thousands of instances, and the closure
		-- allocation was pure overhead on the teardown path.
		pcall(c.Disconnect, c)
	end
	scaleState.trackedConnectionCount = math.max(0, scaleState.trackedConnectionCount - #bucket)
	scaleState.trackedInstanceCount = math.max(0, scaleState.trackedInstanceCount - 1)
	connections[inst] = nil
	activeConnectionSet[inst] = nil
	avoidAttributeConnectionSet[inst] = nil
	baseConnectionSet[inst] = nil
	lastAvoidSyncValue[inst] = nil
	lastKnownName[inst] = nil
	scaleState.lastKnownParent[inst] = nil
	materializedContainers[inst] = nil
	if scaleState.avoidSyncRoots[inst] then
		scaleState.avoidSyncRoots[inst] = nil
		scaleState.markAvoidSyncPathsDirty()
	end
end

disconnectTree = function(inst)
	local stack = { inst }
	while #stack > 0 do
		local current = table.remove(stack)
		for _, child in ipairs(current:GetChildren()) do
			stack[#stack + 1] = child
		end
		disconnectInstance(current)
	end
end

connectInstance = function(inst, excluded, relevant)
	local bucket = connections[inst]
	if not bucket then
		bucket = scaleState.emptyConnectionBucket
		connections[inst] = bucket
		scaleState.trackedInstanceCount += 1
		scaleState.lastKnownParent[inst] = inst.Parent
		lastKnownName[inst] = inst.Name
		lastAvoidSyncValue[inst] = hasAvoidSyncAttribute(inst)
	end

	-- ItemChanged is one DataModel-wide property signal. When Studio exposes
	-- it, keep only lightweight identity metadata per Instance instead of
	-- allocating Parent/Name/Source/attribute RBXScriptConnections tens of
	-- thousands of times.
	if scaleState.itemChangedAvailable then
		if hasAvoidSyncAttribute(inst) then
			scaleState.avoidSyncRoots[inst] = true
			scaleState.markAvoidSyncPathsDirty()
		end
		return
	end

	-- Fallback Studio channels lack ItemChanged. Parent/Name/Source signals are
	-- still needed only for scripts and projected/materialized ancestors.
	-- AvoidSync itself is covered by one generation-scoped scanner below,
	-- avoiding an attribute RBXScriptConnection on every Instance.
	if not relevant then
		return
	end
	if bucket == scaleState.emptyConnectionBucket then
		bucket = {}
		connections[inst] = bucket
	end
	if not baseConnectionSet[inst] then
		table.insert(
			bucket,
			inst:GetPropertyChangedSignal("Parent"):Connect(function()
				scaleState.onParentChanged(inst)
			end)
		)
		scaleState.trackedConnectionCount += 1
		table.insert(
			bucket,
			inst:GetPropertyChangedSignal("Name"):Connect(function()
				onChanged(inst, "Name")
			end)
		)
		scaleState.trackedConnectionCount += 1
		baseConnectionSet[inst] = true
	end

	if excluded or activeConnectionSet[inst] then
		if hasAvoidSyncAttribute(inst) then
			scaleState.avoidSyncRoots[inst] = true
			scaleState.markAvoidSyncPathsDirty()
		end
		return
	end

	activeConnectionSet[inst] = true
	if inst:IsA("LuaSourceContainer") then
		table.insert(
			bucket,
			inst:GetPropertyChangedSignal("Source"):Connect(function()
				onChanged(inst, "Source")
			end)
		)
		scaleState.trackedConnectionCount += 1
	end
	if hasAvoidSyncAttribute(inst) then
		scaleState.avoidSyncRoots[inst] = true
		scaleState.markAvoidSyncPathsDirty()
	end
end

scaleState.generatedPrefixState = function(inst: Instance): (number, boolean)
	if not wallyRootSegments or #wallyRootSegments == 0 then
		return -1, false
	end
	local path = pathFromRoot(inst)
	local prefixLength = #wallyRootSegments
	local comparable = math.min(#path, prefixLength)
	for i = 1, comparable do
		if path[i] ~= wallyRootSegments[i] then
			return -1, false
		end
	end
	return comparable, #path >= prefixLength
end

-- One iterative pass installs property hooks and seeds materialized containers.
-- Inherited AvoidSync and generated-root state travel down the stack, avoiding
-- an ancestor walk for every Instance.
connectTree = function(inst, progress, shouldContinue): boolean
	if shouldContinue and not shouldContinue() then
		return false
	end
	local prefixDepth, generated = scaleState.generatedPrefixState(inst)
	local initialPath = pathFromRoot(inst)
	local initialExcluded = generated or scaleState.syncExcludedAt(inst, inst.Parent, initialPath)
	local meter = progress or { visited = 0 }
	local stack = {
		{
			inst = inst,
			entered = false,
			prefixDepth = prefixDepth,
			excluded = initialExcluded,
		},
	}
	while #stack > 0 do
		local frame = stack[#stack]
		if not frame.entered then
			frame.entered = true
			frame.nextChild = 1
			frame.hasScript = false
			frame.blocked = frame.excluded
				or SUPPRESS_CLASSES[frame.inst.ClassName] == true
				or hasAvoidSyncAttribute(frame.inst)
			connectInstance(frame.inst, frame.blocked, frame.blocked or frame.inst:IsA("LuaSourceContainer"))
			meter.visited += 1
			if meter.visited % scaleState.hookYieldEvery == 0 then
				task.wait()
				if shouldContinue and not shouldContinue() then
					return false
				end
			end
			frame.hasScript = not frame.blocked and frame.inst:IsA("LuaSourceContainer")
			-- Walk excluded descendants too, but install only their lightweight
			-- Parent/Name hooks. This preserves moves out of ignored trees and
			-- keeps nested AvoidSync paths current after an excluded rename.
			frame.children = frame.inst:GetChildren()
		end

		local child = frame.children[frame.nextChild]
		if child then
			frame.nextChild += 1
			local childPrefixDepth = -1
			local childGenerated = false
			if frame.prefixDepth >= 0 and wallyRootSegments then
				local expected = frame.prefixDepth + 1
				if wallyRootSegments[expected] == child.Name then
					childPrefixDepth = expected
					childGenerated = expected >= #wallyRootSegments
				end
			end
			stack[#stack + 1] = {
				inst = child,
				entered = false,
				prefixDepth = childPrefixDepth,
				excluded = frame.blocked or childGenerated,
			}
		else
			if frame.hasScript and (frame.inst.ClassName == "Folder" or not ALLOWED_CLASSES[frame.inst.ClassName]) then
				materializedContainers[frame.inst] = true
			end
			if frame.hasScript then
				connectInstance(frame.inst, frame.blocked, true)
			end
			table.remove(stack)
			local parent = stack[#stack]
			if parent and frame.hasScript then
				parent.hasScript = true
			end
		end
	end
	return not shouldContinue or shouldContinue()
end

disconnectHooks = function()
	local previousPause = scaleState.avoidSyncTrackingPaused
	scaleState.avoidSyncTrackingPaused = true
	scaleState.hookGeneration += 1
	for _, c in ipairs(serviceConnections) do
		pcall(c.Disconnect, c)
	end
	serviceConnections = {}
	scaleState.itemChangedAvailable = false
	sourceSyncState.pending = {}
	sourceSyncState.expected = {}
	sourceSyncState.generation = {}
	-- Drain over a snapshot of the keys rather than re-probing `next(connections)`
	-- each iteration. The old form depended on disconnectInstance always removing
	-- the key it was handed; any key that survived made this loop spin forever and
	-- Studio killed the plugin with "script exhaustion time exceeded" while merely
	-- disconnecting. A snapshot terminates in O(n) regardless.
	local hooked = {}
	for inst in pairs(connections) do
		hooked[#hooked + 1] = inst
	end
	for _, inst in ipairs(hooked) do
		disconnectInstance(inst)
	end
	connections = {}
	table.clear(activeConnectionSet)
	table.clear(avoidAttributeConnectionSet)
	table.clear(baseConnectionSet)
	table.clear(lastAvoidSyncValue)
	table.clear(scaleState.lastKnownParent)
	table.clear(scaleState.avoidSyncRoots)
	scaleState.avoidSyncTrackingPaused = previousPause
end

scaleState.requestControlledLiveResync = function(reason)
	if scaleState.liveResyncPending or not reconnectState.desired then
		return
	end
	scaleState.liveResyncPending = true
	log("live sync: " .. tostring(reason) .. " — restarting with bounded initial comparison")
	pendingOps = {}
	pushPending = false
	disconnectHooks()
	connected = false
	busy = true
	reconnectState.pluginCapability = nil
	reconnectState.comparedCapability = nil
	wsGeneration += 1
	local client = ws
	ws = nil
	if client then
		pcall(function()
			client:Close()
		end)
	end
	setPill("connecting")
	if type(reconnectState.retryInitialCompare) == "function" then
		reconnectState.retryInitialCompare(reason)
	end
end

scaleState.startAvoidSyncFallbackScanner = function()
	if scaleState.itemChangedAvailable then
		return
	end
	local generation = scaleState.hookGeneration
	task.spawn(function()
		while
			reconnectState.desired
			and (busy or connected)
			and generation == scaleState.hookGeneration
			and not scaleState.itemChangedAvailable
		do
			local visited = 0
			for inst in pairs(connections) do
				if generation ~= scaleState.hookGeneration or not reconnectState.desired then
					return
				end
				local current = hasAvoidSyncAttribute(inst)
				if current ~= (lastAvoidSyncValue[inst] == true) then
					onAvoidSyncChanged(inst)
				end
				visited += 1
				if visited % scaleState.hookYieldEvery == 0 then
					task.wait()
					if
						generation ~= scaleState.hookGeneration
						or not reconnectState.desired
						or (not busy and not connected)
					then
						return
					end
				end
			end
			task.wait(0.5)
		end
	end)
end

local function hookService(svc, progress, shouldContinue): boolean
	-- The service itself is a root — connect its descendants individually so
	-- each can be torn down on removal. Structural events are service-wide.
	table.insert(
		serviceConnections,
		svc.DescendantAdded:Connect(function(descendant)
			if connections[descendant] then
				return
			end
			-- A subtree add produces one event per descendant. Walk to the
			-- highest untracked ancestor so the subtree is connected/emitted once.
			local root = descendant
			local parent = root.Parent
			while parent and parent ~= svc and not connections[parent] do
				root = parent
				parent = root.Parent
			end
			if connections[root] then
				return
			end
			onDescendantAdded(root)
			connectTree(root)
		end)
	)
	table.insert(
		serviceConnections,
		svc.DescendantRemoving:Connect(function(descendant)
			if not connections[descendant] then
				return
			end
			onDescendantRemoving(descendant)
			disconnectTree(descendant)
		end)
	)
	for _, child in ipairs(svc:GetChildren()) do
		if not connectTree(child, progress, shouldContinue) then
			return false
		end
	end
	return not shouldContinue or shouldContinue()
end

local function installHooks(shouldContinue): boolean
	scaleState.avoidSyncTrackingPaused = true
	disconnectHooks()
	table.clear(scaleState.avoidSyncRoots)
	if shouldContinue and not shouldContinue() then
		scaleState.avoidSyncTrackingPaused = false
		disconnectHooks()
		return false
	end
	local itemChangedOk, itemChangedConnection = pcall(function()
		return game.ItemChanged:Connect(function(item, descriptor)
			if typeof(item) ~= "Instance" or not isUnderWatched(item) then
				return
			end
			local propertyName = if type(descriptor) == "string" then descriptor else nil
			if not propertyName and descriptor then
				pcall(function()
					propertyName = descriptor.Name
				end)
			end
			if propertyName == "Parent" then
				scaleState.onParentChanged(item)
			elseif propertyName == "Name" then
				onChanged(item, "Name")
			elseif propertyName == "Source" and item:IsA("LuaSourceContainer") then
				onChanged(item, "Source")
			elseif type(propertyName) == "string" and string.find(propertyName, "Attributes", 1, true) == 1 then
				local current = hasAvoidSyncAttribute(item)
				local previous = lastAvoidSyncValue[item] == true
				lastAvoidSyncValue[item] = current
				if current ~= previous then
					onAvoidSyncChanged(item)
				end
			end
		end)
	end)
	if itemChangedOk and itemChangedConnection then
		scaleState.itemChangedAvailable = true
		table.insert(serviceConnections, itemChangedConnection)
	end
	local progress = { visited = 0 }
	for _, name in ipairs(WATCHED_SERVICES) do
		if shouldContinue and not shouldContinue() then
			scaleState.avoidSyncTrackingPaused = false
			disconnectHooks()
			return false
		end
		local ok, svc = pcall(function()
			return game:GetService(name)
		end)
		if ok and svc then
			if not hookService(svc, progress, shouldContinue) then
				scaleState.avoidSyncTrackingPaused = false
				disconnectHooks()
				return false
			end
		else
			log("skip service: " .. name)
		end
	end
	if shouldContinue and not shouldContinue() then
		scaleState.avoidSyncTrackingPaused = false
		disconnectHooks()
		return false
	end
	if ScriptEditorService then
		table.insert(
			serviceConnections,
			ScriptEditorService.TextDocumentDidChange:Connect(function(document, _changes)
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
	scaleState.avoidSyncTrackingPaused = false
	scaleState.startAvoidSyncFallbackScanner()
	scaleState.markAvoidSyncPathsDirty()
	log(
		string.format(
			"hooks indexed: %d instance(s), %d property connection(s)",
			scaleState.trackedInstanceCount,
			scaleState.trackedConnectionCount
		)
	)
	if shouldContinue and not shouldContinue() then
		disconnectHooks()
		return false
	end
	return true
end

--------------------------------------------------------------------------------
-- Studio snapshot + stats (single bottom-up pass for initial-compare)
--------------------------------------------------------------------------------

scaleState.beginStudioScanGuard = function(opts)
	local guard = {
		dirty = false,
		reason = nil,
		connections = {},
		expectedMutations = {},
	}
	guard.pushExpectedMutation = function(expected)
		guard.expectedMutations[#guard.expectedMutations + 1] = expected
	end
	guard.popExpectedMutation = function(expected)
		local top = #guard.expectedMutations
		if guard.expectedMutations[top] == expected then
			table.remove(guard.expectedMutations)
			return
		end
		for index = top, 1, -1 do
			if guard.expectedMutations[index] == expected then
				table.remove(guard.expectedMutations, index)
				return
			end
		end
	end
	local function isExpected(item, eventKind, eventValue)
		for index = #guard.expectedMutations, 1, -1 do
			local expected = guard.expectedMutations[index]
			if eventKind == "recording" and expected.recording == true then
				return true
			end
			if typeof(item) == "Instance" and typeof(expected.instance) == "Instance" then
				local exact = item == expected.instance
				local inExpectedSubtree = false
				if not exact and expected.structureSubtree == true then
					pcall(function()
						inExpectedSubtree = item:IsDescendantOf(expected.instance)
					end)
				end
				if
					(eventKind == "structure" and expected.structure == true and (exact or inExpectedSubtree))
					or (eventKind == "Parent" and expected.parent == true and exact)
					or (eventKind == "Name" and expected.name == true and exact)
					or (
						eventKind == "Source"
						and expected.source == true
						and exact
						and type(eventValue) == "string"
						and sourcesMatchForApply(eventValue, expected.sourceValue)
					)
				then
					return true
				end
			end
		end
		return false
	end
	local function markDirty(reason, item, eventKind, eventValue)
		if isExpected(item, eventKind, eventValue) then
			return
		end
		guard.dirty = true
		guard.reason = guard.reason or reason
	end
	local function addConnection(connection)
		if connection then
			guard.connections[#guard.connections + 1] = connection
		end
	end

	-- Structural changes are available as one pair of signals per service.
	for _, name in ipairs(WATCHED_SERVICES) do
		local ok, service = pcall(function()
			return game:GetService(name)
		end)
		if ok and service then
			addConnection(service.DescendantAdded:Connect(function(item)
				markDirty("structure added", item, "structure")
			end))
			addConnection(service.DescendantRemoving:Connect(function(item)
				markDirty("structure removed", item, "structure")
			end))
		end
	end

	-- ItemChanged is a single DataModel-wide property signal in Studio. Keep
	-- fallbacks below because some Studio channels do not expose it to plugins.
	local itemChangedOk, itemChangedConnection = pcall(function()
		return game.ItemChanged:Connect(function(item, descriptor)
			local propertyName = if type(descriptor) == "string" then descriptor else nil
			if not propertyName and descriptor then
				pcall(function()
					propertyName = descriptor.Name
				end)
			end
			local snapshotRelevant = propertyName == "Name"
				or propertyName == "Source"
				or propertyName == "Parent"
				or (type(propertyName) == "string" and string.find(propertyName, "Attributes", 1, true) == 1)
			if snapshotRelevant and typeof(item) == "Instance" and isUnderWatched(item) then
				local propertyValue = if propertyName == "Source" then readScriptSource(item) else nil
				markDirty("snapshot property changed", item, propertyName, propertyValue)
			end
		end)
	end)
	if itemChangedOk then
		addConnection(itemChangedConnection)
	end
	if ScriptEditorService then
		addConnection(ScriptEditorService.TextDocumentDidChange:Connect(function(document)
			local scriptInst = scriptFromDocument(document)
			if scriptInst and isUnderWatched(scriptInst) then
				markDirty("script source changed", scriptInst, "Source", sourceFromDocument(document, scriptInst))
			end
		end))
	end
	if not itemChangedOk then
		local recordingOk, recordingConnection = pcall(function()
			return ChangeHistoryService.OnRecordingFinished:Connect(function()
				markDirty("Studio recording finished", nil, "recording")
			end)
		end)
		if recordingOk then
			addConnection(recordingConnection)
		end
	end
	return guard
end

scaleState.disconnectStudioScanGuard = function(guard)
	if not guard then
		return
	end
	for _, connection in ipairs(guard.connections or {}) do
		pcall(function()
			connection:Disconnect()
		end)
	end
	guard.connections = {}
end

scaleState.finishStudioScanFrame = function(frame, stats)
	if frame.suppressed then
		return nil, false
	end
	if frame.avoidMarker then
		return avoidSyncMarker(frame.inst), false
	end

	local isScript = frame.inst:IsA("LuaSourceContainer")
	local syncable = isScript or frame.hasSyncableChild == true
	local carriesAvoidSync = not syncable and #frame.outputChildren > 0
	if not frame.serviceRoot and not syncable and not carriesAvoidSync then
		return nil, false
	end

	local allowed = ALLOWED_CLASSES[frame.inst.ClassName] == true
	if carriesAvoidSync and not frame.serviceRoot then
		local carrier = {
			class = "Folder",
			name = frame.inst.Name,
			properties = {},
			children = frame.outputChildren,
			avoidSyncCarrier = true,
		}
		return carrier, false
	end
	local node = {
		class = allowed and frame.inst.ClassName or "Folder",
		name = frame.inst.Name,
		properties = allowed and serializeProperties(frame.inst) or {},
		children = frame.outputChildren,
	}
	if not frame.serviceRoot then
		stats.instanceCount += 1
		if isScript then
			stats.scriptCount += 1
		end
	end
	if frame.inst.ClassName == "Folder" or not allowed then
		materializedContainers[frame.inst] = true
	end
	return node, syncable
end

scaleState.scanStudioService = function(service: Instance, stats, progress)
	local rootNode = nil
	local stack = {
		{
			inst = service,
			serviceRoot = true,
			entered = false,
		},
	}
	while #stack > 0 do
		local frame = stack[#stack]
		if not frame.entered then
			frame.entered = true
			frame.nextChild = 1
			frame.outputChildren = {}
			frame.hasSyncableChild = false
			progress.visited += 1
			if progress.visited % scaleState.studioScanYieldEvery == 0 then
				task.wait()
			end
			if SUPPRESS_CLASSES[frame.inst.ClassName] then
				frame.suppressed = true
				frame.children = {}
			elseif hasAvoidSyncAttribute(frame.inst) then
				frame.avoidMarker = true
				frame.children = {}
			else
				frame.children = frame.inst:GetChildren()
			end
		end

		local child = frame.children[frame.nextChild]
		if child then
			frame.nextChild += 1
			if not SUPPRESS_CLASSES[child.ClassName] then
				stack[#stack + 1] = {
					inst = child,
					serviceRoot = false,
					entered = false,
				}
			end
		else
			local node, syncable = scaleState.finishStudioScanFrame(frame, stats)
			table.remove(stack)
			local parent = stack[#stack]
			if parent then
				if node then
					parent.outputChildren[#parent.outputChildren + 1] = node
				end
				if syncable then
					parent.hasSyncableChild = true
				end
			else
				rootNode = node
			end
		end
	end
	return rootNode
end

scaleState.scanStudioServiceStats = function(service: Instance, stats, progress, projectionInfo)
	local stack = {
		{
			inst = service,
			serviceRoot = true,
			entered = false,
			childIndex = 0,
		},
	}
	while #stack > 0 do
		local frame = stack[#stack]
		if not frame.entered then
			frame.entered = true
			frame.nextChild = 1
			frame.hasSyncableChild = false
			frame.hasAvoidSyncChild = false
			frame.projectedChildCount = 0
			progress.visited += 1
			if progress.visited % scaleState.studioScanYieldEvery == 0 then
				task.wait()
			end
			if SUPPRESS_CLASSES[frame.inst.ClassName] then
				frame.children = {}
				frame.suppressed = true
			elseif hasAvoidSyncAttribute(frame.inst) then
				frame.children = {}
				frame.avoidMarker = true
				frame.actualHasChildren = #frame.inst:GetChildren() > 0
			else
				frame.children = frame.inst:GetChildren()
			end
		end

		local child = frame.children[frame.nextChild]
		if child then
			local childIndex = frame.nextChild
			frame.nextChild += 1
			stack[#stack + 1] = {
				inst = child,
				serviceRoot = false,
				entered = false,
				childIndex = childIndex,
			}
		else
			local projected = false
			local syncable = false
			local carriesAvoidSync = false
			if frame.avoidMarker then
				projected = true
				carriesAvoidSync = true
				scaleState.avoidSyncRoots[frame.inst] = true
				local mappedClass = if ALLOWED_CLASSES[frame.inst.ClassName] then frame.inst.ClassName else "Folder"
				projectionInfo[frame.inst] = {
					class = mappedClass,
					avoidSync = true,
					avoidSyncCarrier = false,
					childCount = 0,
					-- Folder projections always have directory shape on disk,
					-- even when the ignored reservation is currently empty.
					hasChildren = mappedClass == "Folder" or frame.actualHasChildren == true,
					childIndex = frame.childIndex,
				}
			elseif not frame.suppressed then
				local isScript = frame.inst:IsA("LuaSourceContainer")
				syncable = isScript or frame.hasSyncableChild
				local carrier = not syncable and frame.hasAvoidSyncChild
				projected = frame.serviceRoot or syncable or carrier
				carriesAvoidSync = frame.hasAvoidSyncChild
				if projected then
					local mappedClass = if carrier
						then "Folder"
						elseif frame.serviceRoot then frame.inst.ClassName
						elseif ALLOWED_CLASSES[frame.inst.ClassName] then frame.inst.ClassName
						else "Folder"
					projectionInfo[frame.inst] = {
						class = mappedClass,
						avoidSync = false,
						avoidSyncCarrier = carrier,
						childCount = frame.projectedChildCount,
						hasChildren = frame.projectedChildCount > 0,
						childIndex = frame.childIndex,
					}
				end
				if syncable and not frame.serviceRoot then
					stats.instanceCount += 1
					if isScript then
						stats.scriptCount += 1
					end
				end
			end
			table.remove(stack)
			local parent = stack[#stack]
			if parent then
				if projected then
					parent.projectedChildCount += 1
				end
				if syncable then
					parent.hasSyncableChild = true
				end
				if carriesAvoidSync then
					parent.hasAvoidSyncChild = true
				end
			end
		end
	end
end

scaleState.collectStudioStatsWithGuard = function(guard)
	local stats = { scriptCount = 0, instanceCount = 0 }
	local progress = { visited = 0 }
	-- Strong keys, deliberately. Roblox collects an Instance's userdata once no
	-- other Lua reference exists — even while the instance is still in the
	-- DataModel — so a weak-keyed entry can vanish between the stats scan and
	-- the streamed handshake that reads it back. The structure stream then
	-- silently drops the evicted children while their parent still declares
	-- scan-time childCount, and the daemon rejects the whole push ("streamed
	-- parent N declared X children but received Y"). Lifetime stays bounded:
	-- the owning scan guard is disposed on every handshake exit.
	local projectionInfo = {}
	if guard then
		guard.projectionInfo = projectionInfo
	end
	for _, name in ipairs(WATCHED_SERVICES) do
		local ok, service = pcall(function()
			return game:GetService(name)
		end)
		if ok and service then
			scaleState.scanStudioServiceStats(service, stats, progress, projectionInfo)
		end
		if guard and guard.dirty then
			break
		end
	end
	return stats, progress
end

-- Protocol 6 emits source-free, parent-linked records directly from the
-- projection index. It never materializes a nested service table, and keeps
-- each JSON request bounded even when one service owns the whole project.
scaleState.streamStudioServiceStructure = function(service: Instance, guard, sendChunk)
	local projectionInfo = guard and guard.projectionInfo
	if type(projectionInfo) ~= "table" or not projectionInfo[service] then
		return nil, "Studio projection index is unavailable"
	end
	local orderingContext = snapshotApplyState.newContext({
		indexAvoidSyncCarriers = true,
	})
	local records = {}
	local sourceInstances = {}
	local instanceById = {}
	local nextId = 0
	local chunkIndex = 0
	local function flush(finalChunk)
		local chunk = records
		records = {}
		local ok, err, nextChunkIndex = sendChunk(chunkIndex, finalChunk, chunk)
		if not ok then
			return false, err
		end
		chunkIndex = if type(nextChunkIndex) == "number" then nextChunkIndex else chunkIndex + 1
		return true
	end
	local stack = {
		{
			inst = service,
			parentId = nil,
			childIndex = 0,
			depth = 0,
			entered = false,
		},
	}
	while #stack > 0 do
		if guard and guard.dirty then
			return nil, "Studio changed during streamed structure scan"
		end
		local frame = stack[#stack]
		if not frame.entered then
			if frame.depth > scaleState.maxStreamDepth then
				return nil,
					string.format(
						"Studio projection exceeds the supported depth of %d instances",
						scaleState.maxStreamDepth
					)
			end
			if nextId >= scaleState.maxStreamNodes then
				return nil,
					string.format(
						"Studio projection exceeds the supported limit of %d instances",
						scaleState.maxStreamNodes
					)
			end
			frame.entered = true
			frame.id = nextId
			nextId += 1
			instanceById[frame.id + 1] = frame.inst
			if nextId % scaleState.studioScanYieldEvery == 0 then
				task.wait()
				if guard and guard.dirty then
					return nil, "Studio changed during streamed structure scan"
				end
			end
			local info = projectionInfo[frame.inst]
			if not info then
				return nil, "Studio projection changed while streaming"
			end
			if #records >= scaleState.structureChunkRecords then
				local ok, err = flush(false)
				if not ok then
					return nil, err
				end
			end
			records[#records + 1] = {
				id = frame.id,
				parentId = frame.parentId,
				childIndex = frame.childIndex,
				childCount = info.childCount,
				hasChildren = info.hasChildren == true,
				name = frame.inst.Name,
				class = info.class,
				avoidSync = info.avoidSync == true,
				avoidSyncCarrier = info.avoidSyncCarrier == true,
			}
			if frame.inst:IsA("LuaSourceContainer") and not info.avoidSync and not info.avoidSyncCarrier then
				sourceInstances[#sourceInstances + 1] = {
					id = frame.id,
					inst = frame.inst,
				}
			end
			frame.children = {}
			local physicalSiblings = snapshotApplyState.buildPhysicalSiblingIndex(frame.inst, orderingContext, {})
			for _, child in ipairs(frame.inst:GetChildren()) do
				if projectionInfo[child] then
					local placement = physicalSiblings.byInstance[child]
					if not placement then
						return nil, "Studio projection could not allocate a canonical child identity"
					end
					frame.children[#frame.children + 1] = {
						inst = child,
						fragment = placement.fragment,
						order = placement.order,
					}
				end
			end
			table.sort(frame.children, function(left, right)
				if left.order ~= right.order then
					return left.order < right.order
				end
				return left.fragment < right.fragment
			end)
			frame.nextChild = 1
		end
		local childEntry = frame.children[frame.nextChild]
		if childEntry then
			local childIndex = frame.nextChild - 1
			frame.nextChild += 1
			stack[#stack + 1] = {
				inst = childEntry.inst,
				parentId = frame.id,
				childIndex = childIndex,
				depth = frame.depth + 1,
				entered = false,
			}
		else
			table.remove(stack)
		end
	end
	local ok, err = flush(true)
	if not ok then
		return nil, err
	end
	return sourceInstances, nil, instanceById
end

scaleState.normalizedSourceHash = function(inst: Instance): (string?, string?)
	local source, sourceErr = tryReadScriptSource(inst)
	if source == nil then
		return nil, sourceErr
	end
	if string.find(source, "\r\n", 1, true) then
		source = string.gsub(source, "\r\n", "\n")
	end
	return sha256Hex(source), nil
end

scaleState.collectStudioState = function()
	local attempt = 0
	while true do
		attempt += 1
		local guard = scaleState.beginStudioScanGuard()
		local started = os.clock()
		local stats = { scriptCount = 0, instanceCount = 0 }
		local progress = { visited = 0 }
		local services = {}
		for _, name in ipairs(WATCHED_SERVICES) do
			local ok, service = pcall(function()
				return game:GetService(name)
			end)
			if ok and service then
				local node = scaleState.scanStudioService(service, stats, progress)
				if node then
					services[#services + 1] = node
				end
			end
		end
		if not guard.dirty then
			log(
				string.format(
					"Studio scan: %d visited, %d synced path(s), %d script(s), %.2fs%s",
					progress.visited,
					stats.instanceCount,
					stats.scriptCount,
					os.clock() - started,
					attempt > 1 and string.format(" (%d attempts)", attempt) or ""
				)
			)
			return stats, services, guard
		end
		scaleState.disconnectStudioScanGuard(guard)
		log(
			string.format(
				"Studio changed during snapshot (%s); retrying coherent scan",
				tostring(guard.reason or "unknown change")
			)
		)
		task.wait()
	end
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
local WS_OPEN_TIMEOUT = 10
local WS_HEARTBEAT_INTERVAL = 20
local WS_HEARTBEAT_TIMEOUT = 45
local WS_POST_WAKE_GRACE = 30
-- A scheduling gap longer than the normal liveness deadline usually means
-- Studio or macOS was asleep/frozen. Send a fresh probe and grant one complete
-- heartbeat window instead of immediately judging pre-stall traffic stale.
local WS_SCHEDULER_STALL_THRESHOLD = WS_HEARTBEAT_TIMEOUT

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

local function parseWsPayload(payload): (any, number)
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
		return nil, 0
	end
	if not text or text == "" then
		return nil, 0
	end
	local decoded
	local decOk, err = pcall(function()
		decoded = HttpService:JSONDecode(text)
	end)
	if not decOk then
		log("ws decode err: " .. tostring(err))
		return nil, 0
	end
	if type(decoded) ~= "table" then
		return nil, 0
	end
	return decoded, #text
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

local remoteCodec
do
	local moduleScript = script:FindFirstChild("RemoteCodec")
	if not moduleScript or not moduleScript:IsA("ModuleScript") then
		error("[Ro Sync] packaged RemoteCodec module is missing")
	end
	local module = require(moduleScript)
	remoteCodec = module.install({
		resolvePath = resolvePath,
	})
end
local splitRemotePath = remoteCodec.splitPath
local resolveRemotePath = remoteCodec.resolvePath
local instancePathString = remoteCodec.pathOf
local encodeRemoteValue = remoteCodec.encode
local decodeRemoteValue = remoteCodec.decode

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
			buildCommit = PLUGIN_BUILD_COMMIT,
			buildDirty = PLUGIN_BUILD_DIRTY,
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
			build_commit = PLUGIN_BUILD_COMMIT,
			build_dirty = PLUGIN_BUILD_DIRTY,
			studio_version = studioVersion,
		}
	end

	remoteHandlers.ping = function(_args)
		return { pong = os.clock() }
	end

	-- Class, enum, and attribute introspection lives in a packaged child
	-- module so its large fallback metadata table is compiled independently.
	do
		local moduleScript = script:FindFirstChild("Reflection")
		if not moduleScript or not moduleScript:IsA("ModuleScript") then
			error("[Ro Sync] packaged Reflection module is missing")
		end
		local module = require(moduleScript)
		local controller = module.install({
			resolvePath = resolveRemotePath,
			pathOf = instancePathString,
			decode = decodeRemoteValue,
			findLimitDefault = FIND_LIMIT_DEFAULT,
		})
		for name, handler in pairs(controller.handlers) do
			remoteHandlers[name] = handler
		end
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
	if message:lower():find("protocol", 1, true) then
		setPill("error", "update Ro Sync")
		setBanner("Daemon/plugin versions are incompatible. Update Ro Sync and reinstall the Studio plugin.")
	else
		setPill("error", "action required")
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
	local pinnedProject = reconnectState.pinnedProject
	if type(pinnedProject) ~= "string" or pinnedProject == "" then
		return false, "selected daemon project identity is missing; reconnect explicitly", true
	end
	local function matchesPinnedProject(helloValue)
		return type(helloValue) == "table"
			and type(helloValue.project) == "string"
			and helloValue.project == pinnedProject
	end
	local candidateUrl = tostring(baseUrl or ""):gsub("/+$", "")
	local candidatePort = connectedPort
	local hello, helloError = readHello(candidateUrl)
	if hello ~= nil and expectedGameId ~= "0" and tostring(hello.gameId or "") ~= expectedGameId then
		hello = nil
		helloError = "daemon now belongs to a different GameId"
	end
	if hello ~= nil and not matchesPinnedProject(hello) then
		hello = nil
		helloError = "daemon now belongs to a different project"
	end

	if hello == nil and type(reconnectState.discoverDaemon) == "function" then
		local discovered = reconnectState.discoverDaemon(expectedGameId, true, false, pinnedProject)
		if type(discovered) == "table" and discovered.found == true then
			candidateUrl = tostring(discovered.url or ""):gsub("/+$", "")
			candidatePort = tostring(discovered.port or candidatePort or "?")
			hello, helloError = readHello(candidateUrl)
		elseif type(discovered) == "table" and discovered.ambiguous == true then
			return false, "multiple Ro Sync projects match this place; enter the intended daemon URL manually", true
		end
	end

	if hello == nil then
		return false, "daemon hello unavailable: " .. tostring(helloError or "not found"), false
	end
	if expectedGameId ~= "0" and tostring(hello.gameId or "") ~= expectedGameId then
		return false, "daemon hello changed to a different GameId during discovery", false
	end
	if not matchesPinnedProject(hello) then
		return false, "daemon hello changed to a different project during discovery", false
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
			local opened = false
			local openError = nil
			local accepted = false
			local terminalReason = nil
			local lastPongClock = os.clock()
			local heartbeatSuspectClock = nil
			local conns: { RBXScriptConnection } = {}
			local closeStarted = false
			local function closeClient()
				-- Roblox can take an unbounded amount of time to return from
				-- WebStreamClient:Close() (observed at roughly 1,000 seconds
				-- after a watcher shutdown). Publish the local terminal state
				-- first so the watchdog exits and recovery starts immediately,
				-- then isolate the engine close from this callback/task.
				closed = true
				if closeStarted then
					return
				end
				closeStarted = true
				task.spawn(function()
					pcall(function()
						client:Close()
					end)
				end)
			end
			local function acceptTransport()
				if accepted or not reconnectState.desired or not connected or gen ~= wsGeneration then
					if not reconnectState.desired or not connected or gen ~= wsGeneration then
						closeClient()
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

			local function shouldContinueApply()
				return reconnectState.desired and connected and not closed and gen == wsGeneration and ws == client
			end

			-- Inbound filesystem ops enter one cooperative lane, exactly like the
			-- remote-write path already does. Roblox event callbacks overlap
			-- whenever a handler yields, and applyOps yields (UpdateSourceAsync)
			-- while holding a ChangeHistoryService recording — of which Studio
			-- allows exactly one per plugin. A second op frame arriving mid-apply
			-- used to fail its TryBeginRecording, which dropped the socket, which
			-- forced a Studio-authoritative resync that discarded local edits.
			-- Draining every queued op into a single apply removes both the
			-- contention and the per-frame recording overhead.
			-- Connection-scope, deliberately not a top-level local: the main chunk
			-- sits one register under Luau's 200-local ceiling.
			local OP_QUEUE_MAX = 4096
			local OP_FRAME_MAX = 256
			local OP_QUEUE_MAX_BYTES = 16 * 1024 * 1024
			local opQueue = {}
			local opQueueBytes = 0
			local opWorkerRunning = false

			local function runOpWorker()
				if opWorkerRunning then
					return
				end
				opWorkerRunning = true
				task.spawn(function()
					while #opQueue > 0 do
						if not shouldContinueApply() then
							table.clear(opQueue)
							break
						end
						local batch = opQueue
						opQueue = {}
						opQueueBytes = 0
						local batchOk, failedOps = applyOps(batch, shouldContinueApply)
						if batchOk and shouldContinueApply() then
							-- Echo the source Studio actually accepted. The daemon uses
							-- this as the acknowledgement that advances its conflict
							-- baseline; failed/cancelled writes produce no ack. Ops whose
							-- Source Studio rejected are skipped individually so their
							-- siblings still advance.
							local acks = {}
							for _, op in ipairs(batch) do
								if failedOps == nil or not failedOps[op] then
									local ack = sourceAckForAppliedOp(op)
									if ack then
										table.insert(acks, ack)
									end
								end
							end
							if #acks > 0 then
								wsSend(client, { type = "push", ops = acks })
							end
							if failedOps ~= nil and next(failedOps) ~= nil and shouldContinueApply() then
								-- Successful siblings are committed and acknowledged, but
								-- every rejected Source has already been consumed from the
								-- daemon stream. Re-run the bounded comparison so those
								-- scripts are offered again instead of remaining stale.
								scaleState.requestControlledLiveResync("one or more live Source writes were rejected")
							end
						elseif shouldContinueApply() then
							-- The daemon's broadcast is already consumed. Without an
							-- operation ledger, merely withholding the Source ack leaves
							-- Studio stale forever. Fail closed into the bounded compare,
							-- which re-offers the exact disk state without discarding it.
							scaleState.requestControlledLiveResync("live filesystem apply failed")
						end
						-- applyOps only yields when the batch carried Source writes, so
						-- a long run of structural ops (folder creates, deletes, renames)
						-- would otherwise hold this thread for its whole drain and risk
						-- script exhaustion. One yield per batch bounds that.
						task.wait()
					end
					opWorkerRunning = false
				end)
			end

			table.insert(
				conns,
				client.Opened:Connect(function()
					opened = true
				end)
			)
			table.insert(
				conns,
				client.Error:Connect(function(code, message)
					openError = string.format("%s: %s", tostring(code), tostring(message))
					recoveryContext = "WebSocket error " .. openError
					closed = true
				end)
			)
			table.insert(
				conns,
				client.MessageReceived:Connect(function(payload)
					if not reconnectState.desired or not connected or gen ~= wsGeneration then
						closeClient()
						return
					end
					local msg, payloadBytes = parseWsPayload(payload)
					if not msg then
						return
					end
					-- Any server traffic counts as liveness for the heartbeat
					-- watchdog — the daemon pings every 10s on its own schedule,
					-- so we don't need to see a pong specifically.
					lastPongClock = os.clock()
					heartbeatSuspectClock = nil
					local kind = tostring(msg.type or "")
					if
						kind == "op"
						or kind == "ops"
						or kind == "lagged"
						or kind == "pong"
						or kind == "ping"
						or kind == "push-result"
						or kind == "request"
					then
						acceptTransport()
					end
					if kind == "op" or kind == "ops" then
						local incoming = {}
						local malformedFrame = false
						if kind == "op" and type(msg.op) == "table" then
							incoming[1] = msg.op
						elseif
							kind == "ops"
							and type(msg.ops) == "table"
							and #msg.ops > 0
							and #msg.ops <= OP_FRAME_MAX
						then
							for _, op in ipairs(msg.ops) do
								if type(op) ~= "table" then
									malformedFrame = true
									table.clear(incoming)
									break
								end
								incoming[#incoming + 1] = op
							end
						else
							malformedFrame = true
						end
						if malformedFrame then
							log("ws: operation frame was malformed or exceeded its bounded count")
							scaleState.requestControlledLiveResync("malformed or oversized inbound operation frame")
						end
						-- Never apply inline: this callback can be re-entered while a
						-- previous apply is still yielding. Queue and let the single
						-- worker drain it.
						if not malformedFrame and #incoming > 0 then
							if
								#opQueue + #incoming > OP_QUEUE_MAX
								or opQueueBytes + payloadBytes > OP_QUEUE_MAX_BYTES
							then
								log(
									string.format(
										"ws: inbound op queue exceeded %d ops or %d bytes — requesting a controlled resync",
										OP_QUEUE_MAX,
										OP_QUEUE_MAX_BYTES
									)
								)
								table.clear(opQueue)
								opQueueBytes = 0
								scaleState.requestControlledLiveResync("inbound op queue overflow")
							else
								for _, op in ipairs(incoming) do
									table.insert(opQueue, op)
								end
								opQueueBytes += payloadBytes
								runOpWorker()
							end
						end
					elseif kind == "lagged" then
						log("ws: daemon broadcast lagged — will resync via /initial-compare")
						recoveryContext = "daemon broadcast lagged"
						closeClient()
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
						closeClient()
					elseif kind == "pong" then
					-- liveness already refreshed above
					elseif kind == "ping" then
						wsSend(client, { type = "pong" })
					elseif kind == "push-result" then
						local skipped = tonumber(msg.skipped) or 0
						local conflictCount = if type(msg.conflicts) == "table"
							then #msg.conflicts
							else tonumber(msg.conflicts) or 0
						local errorCount = if type(msg.errors) == "table"
							then #msg.errors
							else tonumber(msg.errors) or 0
						if msg.ok == false or skipped > 0 or conflictCount > 0 or errorCount > 0 then
							log(
								string.format(
									"ws push-result incomplete: ok=%s applied=%s skipped=%d conflicts=%d errors=%d",
									tostring(msg.ok),
									tostring(msg.applied),
									skipped,
									conflictCount,
									errorCount
								)
							)
							recoveryContext = string.format(
								"daemon did not fully apply a Studio update (%d skipped, %d conflict(s), %d error(s))",
								skipped,
								conflictCount,
								errorCount
							)
							setBanner(
								"A Studio update was not fully written to disk. Ro Sync is reconciling before continuing."
							)
							closeClient()
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

			-- CreateWebStreamClient is asynchronous. Sending while its state is
			-- still Connecting can return without throwing while dropping the
			-- frame, leaving an established but forever-unidentified socket.
			-- Wait for Roblox's Opened signal before sending the authenticated
			-- hello, while keeping cancellation and failure bounded.
			opened = client.ConnectionState == Enum.WebStreamClientState.Open
			local openDeadline = os.clock() + WS_OPEN_TIMEOUT
			while
				not opened
				and not closed
				and openError == nil
				and reconnectState.desired
				and connected
				and gen == wsGeneration
				and os.clock() < openDeadline
			do
				task.wait(0.05)
			end

			local helloOk = false
			if opened and not closed and openError == nil then
				-- Announce ourselves only after the transport is writable. If
				-- the initial Send fails outright, the common recovery path
				-- below re-runs the full comparison with backoff.
				helloOk = wsSend(client, {
					type = "hello",
					clientId = clientId,
					role = "plugin",
					protocol = PLUGIN_PROTOCOL_VERSION,
					pluginCapability = reconnectState.pluginCapability,
				})
			elseif openError ~= nil then
				log("ws: open error: " .. openError)
			elseif not closed and reconnectState.desired and connected and gen == wsGeneration then
				recoveryContext = "WebSocket open timed out"
				log("ws: open timed out")
				closeClient()
			end
			if helloOk then
				lastPongClock = os.clock()
				-- A pong is the daemon's acknowledgement that it accepted our
				-- authenticated hello. Keep `ws` unpublished until then so queued
				-- filesystem pushes cannot race ahead of authentication.
				if not wsSend(client, { type = "ping" }) then
					closeClient()
				end
				local lastPing = os.clock()
				local lastWatchdogClock = lastPing
				while connected and gen == wsGeneration and not closed do
					task.wait(1)
					local now = os.clock()
					local schedulerGap = now - lastWatchdogClock
					lastWatchdogClock = now
					if schedulerGap > WS_SCHEDULER_STALL_THRESHOLD then
						-- Traffic timestamps from before a long scheduler pause
						-- cannot prove the socket is dead. Start a new probe
						-- window; ordinary scheduled ticks must then observe
						-- server traffic before the normal timeout expires.
						lastPongClock = now
						lastPing = now
						log(string.format("ws: scheduler stalled for %.1fs — probing before timeout", schedulerGap))
						if not wsSend(client, { type = "ping" }) then
							closeClient()
						end
					elseif now - lastPing >= WS_HEARTBEAT_INTERVAL then
						lastPing = now
						wsSend(client, { type = "ping" })
					end
					if not closed and now - lastPongClock > WS_HEARTBEAT_TIMEOUT then
						if heartbeatSuspectClock == nil then
							heartbeatSuspectClock = now
							log("ws: heartbeat silent — probing during post-wake grace")
							wsSend(client, { type = "ping" })
						elseif now - heartbeatSuspectClock > WS_POST_WAKE_GRACE then
							log("ws: heartbeat remained silent after post-wake grace — closing")
							closeClient()
							break
						end
					else
						heartbeatSuspectClock = nil
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
			closeClient()
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
	local generation = wsGeneration
	local waitSec = wsBackoff
	wsBackoff = math.min(wsBackoff * 2, WS_BACKOFF_MAX)
	local lastError = tostring(context or "transport error")
	setPill("reconnecting", waitSec)
	setBanner(
		string.format(
			"Connection interrupted. Waiting for the matching project daemon. Retrying in %ds. Last check: %s",
			waitSec,
			lastError
		)
	)
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

local function startHooksAndWs(): boolean
	if not reconnectState.desired or not busy then
		return false
	end
	local installGeneration = wsGeneration
	local function shouldContinueInstall()
		return reconnectState.desired and busy and installGeneration == wsGeneration
	end
	if not installHooks(shouldContinueInstall) or not shouldContinueInstall() then
		-- installHooks can yield for very large places. A click-to-cancel or
		-- generation change must tear down every partially installed signal
		-- before this path is ever allowed to publish connected=true.
		disconnectHooks()
		pendingOps = {}
		pushPending = false
		return false
	end
	local scanGuard = scaleState.activeScanGuard
	if scanGuard and scanGuard.dirty then
		-- A mutation on a not-yet-hooked branch may have happened while the
		-- time-sliced install was running. Tear down and compare a fresh view.
		disconnectHooks()
		pendingOps = {}
		pushPending = false
		return false
	end
	if not shouldContinueInstall() then
		disconnectHooks()
		pendingOps = {}
		pushPending = false
		return false
	end
	reconnectState.compareRetryPending = false
	scaleState.liveResyncPending = false
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
	-- Always seed (including an empty list) so a restarted daemon cannot retain
	-- stale AvoidSync prefixes. This payload scales with ignored roots, not place size.
	scaleState.avoidSyncPathsDirty = true
	task.spawn(scaleState.pushAvoidSyncPaths)
	return true
end

local function doPushPath(strictStudio: boolean?, scanGuard, initialChoiceId: string?)
	log("initial-compare → push: streaming bounded Studio structure + Sources")
	local strict = strictStudio == true
	if strict and (type(initialChoiceId) ~= "string" or initialChoiceId == "") then
		log("initial-compare → strict push rejected a missing choiceId")
		return false
	end
	local streamId = HttpService:GenerateGUID(false)
	local cursor = {
		service = WATCHED_SERVICES[1],
		phase = "structure",
		chunk = 0,
	}
	local totalRecords = 0
	local totalSources = 0

	local function shouldContinue()
		return reconnectState.desired and busy and (not scanGuard or not scanGuard.dirty)
	end

	local function encodedSize(body): (number?, string?)
		local ok, encoded = pcall(function()
			return HttpService:JSONEncode(body)
		end)
		if not ok then
			return nil, "JSONEncode failed: " .. tostring(encoded)
		end
		return #encoded, nil
	end

	local function validatePartialReceipt(response): (boolean, string?)
		local allowedFields = {
			ok = true,
			action = true,
			streamId = true,
			error = true,
			failedService = true,
			recoveryRequired = true,
			backups = true,
			committedServices = true,
		}
		for field in pairs(response) do
			if allowedFields[field] ~= true then
				return false, "partial receipt contains an unexpected field"
			end
		end
		if
			response.ok ~= false
			or response.action ~= "partial"
			or response.recoveryRequired ~= true
			or response.streamId ~= streamId
		then
			return false, "partial receipt identity flags do not match this push stream"
		end
		if
			type(response.error) ~= "string"
			or response.error == ""
			or #response.error > scaleState.maxRecoveryReceiptStringBytes
			or string.find(response.error, "\0", 1, true)
		then
			return false, "partial receipt error string is missing or exceeds bounds"
		end
		if type(response.failedService) ~= "string" or not WATCHED_SET[response.failedService] then
			return false, "partial receipt failedService is not a watched service"
		end
		if
			type(response.backups) ~= "table"
			or #response.backups > #WATCHED_SERVICES
			or type(response.committedServices) ~= "table"
			or #response.committedServices > #WATCHED_SERVICES
		then
			return false, "partial receipt recovery arrays exceed service bounds"
		end

		local backupSeen = {}
		for index in pairs(response.backups) do
			if type(index) ~= "number" or index % 1 ~= 0 or index < 1 or index > #response.backups then
				return false, "partial receipt backups must be a dense array"
			end
		end
		for index = 1, #response.backups do
			local backup = response.backups[index]
			if
				type(backup) ~= "string"
				or backup == ""
				or #backup > scaleState.maxRecoveryReceiptStringBytes
				or string.find(backup, "\0", 1, true)
				or backupSeen[backup]
			then
				return false, "partial receipt backup path is invalid, duplicate, or exceeds bounds"
			end
			backupSeen[backup] = true
		end

		local committedBackupCount = 0
		for index in pairs(response.committedServices) do
			if type(index) ~= "number" or index % 1 ~= 0 or index < 1 or index > #response.committedServices then
				return false, "partial receipt committedServices must be a dense array"
			end
		end
		for index = 1, #response.committedServices do
			local entry = response.committedServices[index]
			if type(entry) ~= "table" then
				return false, "partial receipt committed service entry is not an object"
			end
			for field in pairs(entry) do
				if field ~= "service" and field ~= "created" and field ~= "backup" and field ~= "recoveryAction" then
					return false, "partial receipt committed service entry contains an unexpected field"
				end
			end
			if
				entry.service ~= WATCHED_SERVICES[index]
				or type(entry.created) ~= "boolean"
				or type(entry.recoveryAction) ~= "string"
			then
				return false, "partial receipt committedServices is not a watched-service prefix"
			end
			if entry.created then
				if entry.backup ~= nil or entry.recoveryAction ~= "removeCreatedService" then
					return false, "created committed service must require removeCreatedService without a backup"
				end
			else
				if
					type(entry.backup) ~= "string"
					or entry.backup == ""
					or #entry.backup > scaleState.maxRecoveryReceiptStringBytes
					or string.find(entry.backup, "\0", 1, true)
					or entry.recoveryAction ~= "restoreBackup"
				then
					return false, "replaced committed service must require restoreBackup with a bounded backup"
				end
				committedBackupCount += 1
				if response.backups[committedBackupCount] ~= entry.backup then
					return false, "partial receipt backups do not match committed restore paths in order"
				end
			end
		end
		if response.failedService ~= WATCHED_SERVICES[#response.committedServices + 1] then
			return false, "partial receipt failedService does not follow its committed service prefix"
		end
		if #response.backups < committedBackupCount or #response.backups > committedBackupCount + 1 then
			return false, "partial receipt may retain at most one extra backup for failedService"
		end
		return true, nil
	end

	local function stopForPartialReceipt(response, label): string
		local recoveryLines = {}
		local committedBackupCount = 0
		for _, entry in ipairs(response.committedServices) do
			if entry.recoveryAction == "restoreBackup" then
				committedBackupCount += 1
				recoveryLines[#recoveryLines + 1] = string.format("Restore %s from: %s", entry.service, entry.backup)
				log(string.format("push recovery restore %s from: %s", entry.service, entry.backup))
			else
				recoveryLines[#recoveryLines + 1] =
					string.format("Remove newly created service directory: %s", entry.service)
				log(string.format("push recovery remove-created-service: %s", entry.service))
			end
		end
		if #response.backups == committedBackupCount + 1 then
			local failedBackup = response.backups[#response.backups]
			recoveryLines[#recoveryLines + 1] = string.format(
				"Restore failed service %s from retained backup: %s",
				response.failedService,
				failedBackup
			)
			log(string.format("push recovery restore failed service %s from: %s", response.failedService, failedBackup))
		end
		if #recoveryLines == 0 then
			recoveryLines[1] = "(preserve the project and inspect .rosync-backups before making further changes)"
		end
		local fatalMessage = string.format(
			"Disk push stopped after a partial daemon commit during %s.\n"
				.. "Failed service: %s\nError: %s\nRequired recovery actions:\n%s",
			label,
			response.failedService,
			response.error,
			table.concat(recoveryLines, "\n")
		)
		reconnectState.stopTerminal(fatalMessage)
		return fatalMessage
	end

	local function postExact(body, label)
		local bodyBytes, encodeErr = encodedSize(body)
		if not bodyBytes then
			return nil, encodeErr
		end
		if bodyBytes > scaleState.maxStreamRequestBytes then
			return nil,
				string.format(
					"%s encodes to %d bytes; stream request maximum is %d",
					label,
					bodyBytes,
					scaleState.maxStreamRequestBytes
				)
		end
		local lastErr = nil
		for attempt = 1, 2 do
			local ok, response = httpJson("/push", "POST", body, true, PROTOCOL_STREAM_MAX_BYTES)
			local attemptErr = nil
			if
				ok
				and type(response) == "table"
				and response.ok == false
				and response.action == "partial"
				and response.recoveryRequired == true
				and response.streamId == streamId
			then
				local validPartial, partialErr = validatePartialReceipt(response)
				if validPartial then
					-- This exact request may be replayed by the daemon, but a
					-- valid partial receipt is already terminal. Surface every
					-- bounded recovery path and never retry this HTTP chunk.
					return nil, stopForPartialReceipt(response, label)
				end
				attemptErr = "malformed matching partial receipt: " .. tostring(partialErr)
			end
			if ok and type(response) == "table" and response.ok == false and response.stale == true then
				return nil, tostring(response.error or "initial Studio choice became stale")
			end
			if not shouldContinue() then
				return nil, "Studio push cancelled or changed"
			end
			if ok and type(response) == "table" and response.ok == true and response.streamId == streamId then
				return response, nil
			end
			if not attemptErr and ok and type(response) == "table" then
				attemptErr = tostring(response.error or "daemon rejected stream chunk")
			elseif not attemptErr then
				attemptErr = tostring(response)
			end
			lastErr = attemptErr
			if attempt < 2 then
				log(string.format("  ↻ %s — exact retry (%s)", label, lastErr:sub(1, 150)))
				task.wait(0.15)
			end
		end
		return nil, lastErr or "stream request failed"
	end

	local function acceptAdvance(response, serviceName, phase, chunkIndex, finalChunk): (boolean, string?)
		local serviceIndex = table.find(WATCHED_SERVICES, serviceName)
		if not serviceIndex then
			return false, "push response referenced an unknown service"
		end
		if response.action == "complete" then
			if response.phase ~= nil or response.nextService ~= nil or response.nextChunk ~= nil then
				return false, "push terminal response mixed action and cursor coordinates"
			end
			local validTerminal = serviceIndex == #WATCHED_SERVICES
				and ((phase == "sources" and finalChunk) or phase == "diskRevalidate")
			if not validTerminal then
				return false, "daemon completed the push stream before final disk revalidation"
			end
			cursor.service = nil
			cursor.phase = "complete"
			cursor.chunk = 0
			return true, nil
		end
		if response.action ~= nil then
			return false, "push response carried an unknown terminal action"
		end

		local expectedService = serviceName
		local expectedPhase = phase
		local expectedChunk = chunkIndex + 1
		local alternatePhase = nil
		local alternateService = serviceName
		local alternateChunk = 0
		if phase == "structure" and finalChunk then
			expectedPhase = "sources"
			expectedChunk = 0
			alternatePhase = "diskFence"
		elseif phase == "diskFence" then
			alternatePhase = "sources"
		elseif phase == "sources" and finalChunk then
			alternatePhase = "diskRevalidate"
			if serviceIndex < #WATCHED_SERVICES then
				expectedService = WATCHED_SERVICES[serviceIndex + 1]
				expectedPhase = "structure"
				expectedChunk = 0
			else
				expectedPhase = "complete"
				expectedChunk = 0
			end
		elseif phase == "diskRevalidate" then
			if serviceIndex < #WATCHED_SERVICES then
				alternateService = WATCHED_SERVICES[serviceIndex + 1]
				alternatePhase = "structure"
			end
		end
		local exactExpected = response.nextService == expectedService
			and response.phase == expectedPhase
			and response.nextChunk == expectedChunk
		local exactAlternate = alternatePhase ~= nil
			and response.nextService == alternateService
			and response.phase == alternatePhase
			and response.nextChunk == alternateChunk
		if exactExpected and expectedPhase == "complete" then
			return false, "push stream requires the sole explicit terminal action"
		end
		if not exactExpected and not exactAlternate then
			return false, string.format("out-of-order push response after %s/%s/%d", serviceName, phase, chunkIndex)
		end
		if exactAlternate then
			cursor.service = alternateService
			cursor.phase = alternatePhase
			cursor.chunk = alternateChunk
		else
			cursor.service = expectedService
			cursor.phase = expectedPhase
			cursor.chunk = expectedChunk
		end
		if response.nextService ~= expectedService and response.nextService ~= alternateService then
			return false, "push response changed service outside an allowed continuation"
		end
		return true, nil
	end

	local function sendBody(body, label): (boolean, string?)
		if cursor.service ~= body.service or cursor.phase ~= body.phase or cursor.chunk ~= body.chunkIndex then
			return false, "plugin push cursor advanced out of order"
		end
		local response, requestErr = postExact(body, label)
		if not response then
			return false, requestErr
		end
		return acceptAdvance(response, body.service, body.phase, body.chunkIndex, body.finalChunk == true)
	end

	local function makeBody(serviceName, phase, chunkIndex, finalChunk)
		return {
			pluginProtocol = PLUGIN_PROTOCOL_VERSION,
			streamId = streamId,
			choiceId = if strict then initialChoiceId else nil,
			service = serviceName,
			phase = phase,
			chunkIndex = chunkIndex,
			finalChunk = finalChunk,
			strict = strict,
		}
	end

	local function utf8ChunkEnd(source, offset, budget): number?
		local stop = math.min(#source, offset + budget)
		while stop > offset and stop < #source do
			local nextByte = string.byte(source, stop + 1)
			if nextByte and nextByte >= 0x80 and nextByte < 0xC0 then
				stop -= 1
			else
				break
			end
		end
		return if stop > offset or offset == #source then stop else nil
	end

	local function drainContinuation(phase, targetPhase): (boolean, string?)
		while cursor.phase == phase do
			if not shouldContinue() then
				return false, "Studio changed during daemon disk continuation"
			end
			local body = makeBody(cursor.service, phase, cursor.chunk, false)
			body.records = {}
			body.sources = {}
			local sent, sendErr =
				sendBody(body, string.format("%s %s tick %d", tostring(cursor.service), phase, body.chunkIndex))
			if not sent then
				return false, sendErr
			end
		end
		if cursor.phase ~= targetPhase then
			return false, string.format("daemon did not advance %s to %s", phase, targetPhase)
		end
		return true, nil
	end

	for serviceIndex, serviceName in ipairs(WATCHED_SERVICES) do
		if not shouldContinue() then
			return false
		end
		if cursor.service ~= serviceName or cursor.phase ~= "structure" or cursor.chunk ~= 0 then
			log("push stream cursor did not begin the expected service")
			return false
		end
		local serviceOk, service = pcall(function()
			return game:GetService(serviceName)
		end)
		if not serviceOk or not service then
			log("push stream could not read service " .. serviceName)
			return false
		end

		local sourceInstances, structureErr = scaleState.streamStudioServiceStructure(
			service,
			scanGuard,
			function(_suggestedChunkIndex, finalChunk, records)
				local recordIndex = 1
				while recordIndex <= #records do
					local take = math.min(scaleState.structureChunkRecords, #records - recordIndex + 1)
					local body = nil
					while take >= 1 do
						local batch = {}
						for offset = 0, take - 1 do
							batch[#batch + 1] = records[recordIndex + offset]
						end
						local finishesInput = recordIndex + take - 1 == #records
						local candidate = makeBody(serviceName, "structure", cursor.chunk, finalChunk and finishesInput)
						candidate.records = batch
						local bytes = encodedSize(candidate)
						if bytes and bytes <= scaleState.maxStreamRequestBytes then
							body = candidate
							break
						end
						take = math.floor(take / 2)
					end
					if not body then
						return false, "one Studio structure record exceeds the bounded request limit"
					end
					local sent, sendErr =
						sendBody(body, string.format("%s structure chunk %d", serviceName, body.chunkIndex))
					if not sent then
						return false, sendErr
					end
					totalRecords += #body.records
					recordIndex += #body.records
				end
				return true, nil, cursor.chunk
			end
		)
		if not sourceInstances then
			log(string.format("push %s structure failed: %s", serviceName, tostring(structureErr):sub(1, 200)))
			return false
		end
		if cursor.phase == "diskFence" then
			local drained, drainErr = drainContinuation("diskFence", "sources")
			if not drained then
				log("push disk fence failed: " .. tostring(drainErr))
				return false
			end
		end
		if cursor.service ~= serviceName or cursor.phase ~= "sources" or cursor.chunk ~= 0 then
			log("daemon did not advance push stream to Sources")
			return false
		end

		if #sourceInstances == 0 then
			local body = makeBody(serviceName, "sources", 0, true)
			body.sources = {}
			local sent, sendErr = sendBody(body, string.format("%s empty Sources", serviceName))
			if not sent then
				log("push source stream failed: " .. tostring(sendErr))
				return false
			end
		else
			-- Pack as many bounded Source parts as fit into every request. The
			-- daemon accepts up to sourcePartChunkRecords parts and a 512 KiB
			-- body per Sources chunk; one request per part made a 1,200-script
			-- push cost 1,200+ HTTP requests, which the HttpService budget
			-- pacing then stretched into minutes.
			local envelopeBody = makeBody(serviceName, "sources", cursor.chunk, false)
			envelopeBody.sources = {}
			local envelopeBytes = encodedSize(envelopeBody)
			if not envelopeBytes then
				log("push source stream failed: could not measure the request envelope")
				return false
			end
			-- Slack covers chunkIndex/finalChunk digit drift between the
			-- envelope measured above and the eventual flush bodies. postExact
			-- still hard-verifies every encoded body against the wire limit.
			local packedBudget = scaleState.maxStreamRequestBytes - envelopeBytes - 64
			local batch = {}
			local batchBytes = 0
			local function flushBatch(finalChunk): (boolean, string?)
				local body = makeBody(serviceName, "sources", cursor.chunk, finalChunk)
				body.sources = batch
				local lastPart = batch[#batch]
				local sent, sendErr =
					sendBody(body, string.format("%s Sources x%d (through id %d)", serviceName, #batch, lastPart.id))
				if not sent then
					return false, sendErr
				end
				batch = {}
				batchBytes = 0
				return true
			end
			for sourceIndex, sourceEntry in ipairs(sourceInstances) do
				if not shouldContinue() then
					return false
				end
				local source, sourceErr = tryReadScriptSource(sourceEntry.inst)
				if source == nil then
					log(
						string.format(
							"push Source %s could not read the Studio editor buffer: %s",
							pathOf(sourceEntry.inst),
							tostring(sourceErr)
						)
					)
					return false
				end
				if #source > scaleState.maxScriptSourceBytes then
					log(
						string.format(
							"push Source %s is %d bytes; maximum is %d",
							pathOf(sourceEntry.inst),
							#source,
							scaleState.maxScriptSourceBytes
						)
					)
					return false
				end
				local sourceHash = sha256Hex(source)
				local offset = 0
				local partIndex = 0
				repeat
					local budget = math.min(scaleState.sourceChunkBytes, math.max(0, #source - offset))
					local part = nil
					local partBytes = nil
					while budget >= 0 do
						local stop = utf8ChunkEnd(source, offset, budget)
						if not stop then
							return false
						end
						local candidate = {
							id = sourceEntry.id,
							partIndex = partIndex,
							offset = offset,
							totalBytes = #source,
							data = string.sub(source, offset + 1, stop),
							finalPart = stop == #source,
							sha256 = sourceHash,
						}
						local bytes = encodedSize(candidate)
						if bytes and bytes <= packedBudget then
							part = candidate
							partBytes = bytes
							break
						end
						if budget == 0 then
							break
						end
						budget = math.floor(budget / 2)
					end
					if not part then
						log("push Source contains invalid UTF-8 or cannot fit one bounded request")
						return false
					end
					-- Flush before appending, never after: the final part of the
					-- final script must ride the finalChunk flush below. A JSON
					-- array's size is the sum of its independently encoded
					-- elements plus one comma per element, so this bound is exact.
					if
						#batch > 0
						and (
							#batch >= scaleState.sourcePartChunkRecords
							or batchBytes + #batch + partBytes > packedBudget
						)
					then
						local flushed, flushErr = flushBatch(false)
						if not flushed then
							log("push source stream failed: " .. tostring(flushErr))
							return false
						end
						if not shouldContinue() then
							return false
						end
					end
					batch[#batch + 1] = part
					batchBytes += partBytes
					offset += #part.data
					partIndex += 1
					if partIndex % 8 == 0 then
						task.wait()
						if not shouldContinue() then
							return false
						end
					end
				until offset == #source and partIndex > 0
				totalSources += 1
				-- Requests are rare now, so yield periodically between scripts to
				-- keep Studio responsive through the hash/encode CPU work.
				if sourceIndex % 16 == 0 then
					task.wait()
					if not shouldContinue() then
						return false
					end
				end
			end
			local flushed, flushErr = flushBatch(true)
			if not flushed then
				log("push source stream failed: " .. tostring(flushErr))
				return false
			end
		end

		if cursor.phase == "diskRevalidate" then
			local targetPhase = if serviceIndex < #WATCHED_SERVICES then "structure" else "complete"
			local drained, drainErr = drainContinuation("diskRevalidate", targetPhase)
			if not drained then
				log("push disk revalidation failed: " .. tostring(drainErr))
				return false
			end
		end

		if serviceIndex < #WATCHED_SERVICES then
			if
				cursor.service ~= WATCHED_SERVICES[serviceIndex + 1]
				or cursor.phase ~= "structure"
				or cursor.chunk ~= 0
			then
				log("daemon advanced push stream to an unexpected service")
				return false
			end
		elseif cursor.phase ~= "complete" then
			log("daemon did not finalize the push stream")
			return false
		end
		snapshotApplyState.seedDiskPathsForService(service)
	end

	log(
		string.format(
			"bootstrap stream done: %d structure record(s), %d Source(s), %d service(s)",
			totalRecords,
			totalSources,
			#WATCHED_SERVICES
		)
	)
	return shouldContinue() and cursor.phase == "complete"
end

local function doPullPath(strictDisk: boolean?, selectiveSelectedCount: number?, selectionChoiceId: string?)
	log("initial-compare → pull: streaming bounded disk structure + Sources")
	local pullGeneration = wsGeneration
	local selective = selectiveSelectedCount ~= nil
	local strict = strictDisk == true and not selective
	if selective then
		if
			type(selectiveSelectedCount) ~= "number"
			or selectiveSelectedCount <= 0
			or selectiveSelectedCount % 1 ~= 0
			or selectiveSelectedCount > scaleState.maxStreamNodes
		then
			log("selective snapshot stream has an invalid authorized selectedCount")
			return false
		end
		if type(selectionChoiceId) ~= "string" or selectionChoiceId == "" then
			log("selective snapshot stream is missing its authorized choiceId")
			return false
		end
	end
	local requestId = HttpService:GenerateGUID(false)
	local streamId = nil
	local mutationGuard = scaleState.beginStudioScanGuard()
	snapshotApplyState.activeMutationGuard = mutationGuard
	local serviceState = nil
	local stagedServiceStates = {}
	local totalStagedSourceBytes = 0

	local function shouldContinue()
		return reconnectState.desired and busy and pullGeneration == wsGeneration and not mutationGuard.dirty
	end

	local function disposeGuard()
		scaleState.disconnectStudioScanGuard(mutationGuard)
		if snapshotApplyState.activeMutationGuard == mutationGuard then
			snapshotApplyState.activeMutationGuard = nil
		end
		for _, state in ipairs(stagedServiceStates) do
			if state.stagingRoot then
				state.stagingRoot:Destroy()
				state.stagingRoot = nil
			end
		end
	end

	local function postExact(body, label)
		local encodedOk, encoded = pcall(function()
			return HttpService:JSONEncode(body)
		end)
		if not encodedOk then
			return nil, "JSONEncode failed: " .. tostring(encoded)
		end
		if #encoded > scaleState.maxStreamRequestBytes then
			return nil, string.format("%s request exceeds %d bytes", label, scaleState.maxStreamRequestBytes)
		end
		local lastErr = nil
		for attempt = 1, 2 do
			local ok, raw = httpRequest("/snapshot/stream", "POST", body, true)
			if not shouldContinue() then
				return nil, mutationGuard.reason or "disk stream cancelled"
			end
			if ok and raw.Success then
				if #(raw.Body or "") > scaleState.maxStreamRequestBytes then
					return nil, string.format("%s response exceeds %d bytes", label, scaleState.maxStreamRequestBytes)
				end
				local decodeOk, decoded = pcall(function()
					return HttpService:JSONDecode(raw.Body or "")
				end)
				if decodeOk and type(decoded) == "table" and decoded.ok == true then
					return decoded, nil
				end
				lastErr = if decodeOk
					then tostring(type(decoded) == "table" and decoded.error or "invalid stream response")
					else "decode: " .. tostring(decoded)
			elseif ok then
				lastErr = string.format("HTTP %d %s", raw.StatusCode, raw.StatusMessage or "")
			else
				lastErr = tostring(raw)
			end
			if attempt < 2 then
				log(string.format("  ↻ %s — exact retry (%s)", label, tostring(lastErr):sub(1, 150)))
				task.wait(0.15)
			end
		end
		return nil, lastErr or "snapshot stream request failed"
	end

	local function integer(value): number?
		if type(value) ~= "number" or value < 0 or value % 1 ~= 0 then
			return nil
		end
		return value
	end

	local function validateStructureChunk(state, records, finalChunk): (boolean, string?)
		if type(records) ~= "table" or #records > scaleState.structureChunkRecords then
			return false, "disk structure chunk is missing or exceeds 512 records"
		end
		if #records == 0 then
			return false, "disk structure chunks may not be empty"
		end
		for _, record in ipairs(records) do
			if type(record) ~= "table" then
				return false, "disk structure record is not an object"
			end
			for field in pairs(record) do
				if
					field ~= "id"
					and field ~= "parentId"
					and field ~= "childIndex"
					and field ~= "childCount"
					and field ~= "hasChildren"
					and field ~= "name"
					and field ~= "class"
					and field ~= "avoidSync"
					and field ~= "avoidSyncCarrier"
					and field ~= "diskFragment"
					and field ~= "diskFragmentIsDir"
					and field ~= "sourceIncluded"
				then
					return false, "disk structure record contains an unexpected field"
				end
			end
			local id = integer(record.id)
			local childIndex = integer(record.childIndex)
			local childCount = integer(record.childCount)
			if id ~= #state.records or not childIndex or not childCount then
				return false, "disk structure IDs/counts are not dense non-negative integers"
			end
			if id >= scaleState.maxStreamNodes then
				return false, string.format("disk structure exceeds %d nodes", scaleState.maxStreamNodes)
			end
			if type(record.name) ~= "string" or string.find(record.name, "\0", 1, true) then
				return false, "disk structure record has an invalid name"
			end
			if type(record.class) ~= "string" then
				return false, "disk structure record has no class"
			end
			if
				type(record.hasChildren) ~= "boolean"
				or type(record.avoidSync) ~= "boolean"
				or type(record.avoidSyncCarrier) ~= "boolean"
				or (record.sourceIncluded ~= nil and type(record.sourceIncluded) ~= "boolean")
			then
				return false, "disk structure flags must be booleans"
			end
			if record.sourceIncluded ~= nil and not SCRIPT_CLASSES[record.class] then
				return false, "sourceIncluded is only valid on script records"
			end
			if id == 0 then
				if
					record.parentId ~= nil
					or childIndex ~= 0
					or record.name ~= state.serviceName
					or record.class ~= state.serviceName
					or record.avoidSync ~= false
					or record.avoidSyncCarrier ~= false
					or record.sourceIncluded ~= nil
					or record.diskFragment ~= nil
					or record.diskFragmentIsDir ~= nil
				then
					return false, "disk structure root does not match its service"
				end
				state.depths[1] = 0
				state.preorderStack[1] = 0
			else
				if not ALLOWED_CLASSES[record.class] then
					return false, "disk structure contains an unsupported class"
				end
				while #state.preorderStack > 0 do
					local topId = state.preorderStack[#state.preorderStack]
					local top = state.records[topId + 1]
					if state.observedChildren[topId + 1] == tonumber(top.childCount) then
						table.remove(state.preorderStack)
					else
						break
					end
				end
				local parentId = integer(record.parentId)
				local expectedParent = state.preorderStack[#state.preorderStack]
				if parentId == nil or parentId ~= expectedParent then
					return false, "disk structure is not dense depth-first preorder"
				end
				local parentObserved = state.observedChildren[parentId + 1] or 0
				if childIndex ~= parentObserved then
					return false, "disk structure childIndex values are not contiguous"
				end
				state.observedChildren[parentId + 1] = parentObserved + 1
				local depth = (state.depths[parentId + 1] or 0) + 1
				if depth > scaleState.maxStreamDepth then
					return false, string.format("disk structure exceeds depth %d", scaleState.maxStreamDepth)
				end
				state.depths[id + 1] = depth
				if type(record.diskFragment) ~= "string" or type(record.diskFragmentIsDir) ~= "boolean" then
					return false, "disk structure record is missing exact fragment identity"
				end
				-- Leaf form and script-with-children directory form are both
				-- valid on-disk representations of a script; a script that lost
				-- its last child stays in directory form until something
				-- rewrites it. Only require a directory when the node cannot be
				-- a single file. See the matching identity-phase check.
				if (record.class == "Folder" or record.hasChildren == true) and record.diskFragmentIsDir ~= true then
					return false, "disk fragment shape does not match its projected node"
				end
				state.preorderStack[#state.preorderStack + 1] = id
			end
			if record.avoidSync == true and record.avoidSyncCarrier == true then
				return false, "disk structure record cannot be both AvoidSync boundary and carrier"
			end
			if record.avoidSync == true and childCount ~= 0 then
				return false, "disk AvoidSync boundary must omit descendants"
			end
			if record.avoidSyncCarrier == true and record.class ~= "Folder" then
				return false, "disk AvoidSync carrier must project as Folder"
			end
			-- The daemon emits hasChildren from the PHYSICAL disk shape while
			-- childCount counts PROJECTED children, so a script left in
			-- directory form after losing its last child reports
			-- hasChildren=true with childCount=0. That is a legal on-disk
			-- state, not a malformed record; demanding equality here rejected
			-- every disk-wins transfer for such a tree. Only require directory
			-- shape when the node cannot be a single file.
			if (id == 0 or childCount > 0 or record.class == "Folder") and record.hasChildren ~= true then
				return false, "disk hasChildren does not match projected shape"
			end
			state.records[#state.records + 1] = record
			state.observedChildren[id + 1] = state.observedChildren[id + 1] or 0
		end
		if finalChunk then
			while #state.preorderStack > 0 do
				local topId = state.preorderStack[#state.preorderStack]
				local top = state.records[topId + 1]
				if state.observedChildren[topId + 1] ~= tonumber(top.childCount) then
					return false, "disk structure ended before all declared children arrived"
				end
				table.remove(state.preorderStack)
			end
			if #state.records == 0 then
				return false, "disk structure did not include its service root"
			end
		end
		return true, nil
	end

	local function stageStructure(state): (boolean, string?)
		local ok, err = pcall(function()
			local stagingRoot = Instance.new("Folder")
			stagingRoot.Name = state.serviceName
			state.stagingRoot = stagingRoot
			state.stagedInstances = { stagingRoot }
			state.childrenByParent = {}
			state.sourceIds = {}
			state.sourceDigests = {}
			state.sourceNormalizedDigests = {}
			state.nextSource = 1

			for id = 1, #state.records - 1 do
				local record = state.records[id + 1]
				local parentId = tonumber(record.parentId)
				local children = state.childrenByParent[parentId + 1]
				if not children then
					children = {}
					state.childrenByParent[parentId + 1] = children
				end
				children[#children + 1] = record
				if
					SCRIPT_CLASSES[record.class]
					and record.avoidSync ~= true
					and record.avoidSyncCarrier ~= true
					and record.sourceIncluded ~= false
				then
					state.sourceIds[#state.sourceIds + 1] = id
				end
				local parent = state.stagedInstances[parentId + 1]
				if not parent then
					error("detached staging parent is missing", 0)
				end
				local staged = Instance.new(record.class)
				staged.Name = record.name
				staged.Parent = parent
				state.stagedInstances[id + 1] = staged
				if id % scaleState.studioScanYieldEvery == 0 then
					task.wait()
					if not shouldContinue() then
						error("detached snapshot staging cancelled", 0)
					end
				end
			end
		end)
		if not ok then
			if state.stagingRoot then
				state.stagingRoot:Destroy()
				state.stagingRoot = nil
			end
			return false, tostring(err)
		end
		return true, nil
	end

	local function acceptSources(state, sources, finalChunk): (boolean, string?)
		if type(sources) ~= "table" or #sources > scaleState.sourcePartChunkRecords then
			return false, "disk source chunk is missing its sources array"
		end
		if #sources == 0 and not finalChunk then
			-- Empty non-final chunks are exact-cursor fence ticks while the
			-- daemon captures or revalidates its disk fingerprint. They are only
			-- legal before the first Source or after every Source; accepting one
			-- in the middle of a script/sequence would hide a stalled stream.
			local beforeFirstSource = state.activeSource == nil and state.nextSource == 1
			local afterEverySource = state.activeSource == nil and state.nextSource > #state.sourceIds
			if not beforeFirstSource and not afterEverySource then
				return false, "disk Source fence tick arrived between ordinary Source parts"
			end
			return true, nil
		end
		for partNumber, part in ipairs(sources) do
			if type(part) ~= "table" or type(part.data) ~= "string" then
				return false, "disk source part is malformed"
			end
			for field in pairs(part) do
				if
					field ~= "id"
					and field ~= "partIndex"
					and field ~= "offset"
					and field ~= "totalBytes"
					and field ~= "data"
					and field ~= "finalPart"
					and field ~= "sha256"
				then
					return false, "disk source part contains an unexpected field"
				end
			end
			local id = integer(part.id)
			local partIndex = integer(part.partIndex)
			local offset = integer(part.offset)
			local totalBytes = integer(part.totalBytes)
			local digest = tostring(part.sha256 or "")
			if
				not id
				or not partIndex
				or not offset
				or not totalBytes
				or #part.data > scaleState.sourceChunkBytes
				or totalBytes > scaleState.maxScriptSourceBytes
				or (#part.data == 0 and totalBytes > 0)
				or type(part.finalPart) ~= "boolean"
				or not string.match(digest, "^[0-9a-f]+$")
				or #digest ~= 64
			then
				return false, "disk source part exceeds bounds or has invalid metadata"
			end
			if not state.activeSource then
				local expectedId = state.sourceIds[state.nextSource]
				if id ~= expectedId or partIndex ~= 0 or offset ~= 0 then
					return false, "disk source stream started out of order"
				end
				if state.stagedSourceBytes + totalBytes > scaleState.maxStagedSourceBytesPerService then
					return false,
						string.format(
							"disk Source staging for %s exceeds the %d-byte per-service limit",
							state.serviceName,
							scaleState.maxStagedSourceBytesPerService
						)
				end
				if totalStagedSourceBytes + totalBytes > scaleState.maxStagedSourceBytesTotal then
					return false,
						string.format(
							"disk Source staging exceeds the %d-byte all-service limit",
							scaleState.maxStagedSourceBytesTotal
						)
				end
				state.stagedSourceBytes += totalBytes
				totalStagedSourceBytes += totalBytes
				state.activeSource = {
					id = id,
					nextPart = 0,
					offset = 0,
					totalBytes = totalBytes,
					sha256 = digest,
					parts = {},
				}
			end
			local active = state.activeSource
			if
				id ~= active.id
				or partIndex ~= active.nextPart
				or offset ~= active.offset
				or totalBytes ~= active.totalBytes
				or digest ~= active.sha256
				or offset + #part.data > totalBytes
			then
				return false, "disk source stream changed identity or offset mid-script"
			end
			local reachesEnd = offset + #part.data == totalBytes
			if (part.finalPart == true) ~= reachesEnd then
				return false, "disk source finalPart does not match totalBytes"
			end
			active.parts[#active.parts + 1] = part.data
			active.offset += #part.data
			active.nextPart += 1
			if reachesEnd then
				local source = table.concat(active.parts)
				if #source ~= totalBytes or sha256Hex(source) ~= digest then
					return false, "disk source SHA-256 validation failed"
				end
				local record = state.records[id + 1]
				local inst = state.stagedInstances[id + 1]
				if not inst or not inst:IsA("LuaSourceContainer") or not record or inst.ClassName ~= record.class then
					return false, "detached source target does not match streamed structure"
				end
				local stagedOk = pcall(function()
					inst.Source = source
				end)
				if not stagedOk or not sourcesMatchForApply(readScriptSource(inst), source) then
					return false, "detached staging rejected streamed Source"
				end
				state.sourceDigests[id + 1] = digest
				state.sourceNormalizedDigests[id + 1] = sha256Hex(normalizeSourceForCompare(source))
				state.activeSource = nil
				state.nextSource += 1
			end
			if partNumber % scaleState.sourcePartChunkRecords == 0 then
				task.wait()
				if not shouldContinue() then
					return false, mutationGuard.reason or "disk Source validation cancelled"
				end
			end
		end
		if finalChunk then
			if state.activeSource or state.nextSource <= #state.sourceIds then
				return false, "disk source phase ended before every script validated"
			end
		end
		return true, nil
	end

	local function acceptDeletes(state, deletes, finalChunk): (boolean, string?)
		if type(deletes) ~= "table" or #deletes > scaleState.deleteChunkRecords then
			return false, "disk delete chunk is missing or exceeds 512 records"
		end
		-- An empty non-final chunk is an authenticated continuation tick while
		-- the daemon re-hashes the exact disk generation in the background.
		-- It advances only the already-validated stream cursor and never opens
		-- a ChangeHistory recording or mutates live Studio state.
		for deleteIndex in pairs(deletes) do
			if type(deleteIndex) ~= "number" or deleteIndex % 1 ~= 0 or deleteIndex < 1 or deleteIndex > #deletes then
				return false, "disk delete chunk must be a dense array"
			end
		end
		for deleteIndex, deletion in ipairs(deletes) do
			if type(deletion) ~= "table" then
				return false, "disk delete record is not an object"
			end
			for field in pairs(deletion) do
				if field ~= "path" and field ~= "pathMode" then
					return false, "disk delete record contains an unexpected field"
				end
			end
			local path = deletion.path
			if
				deletion.pathMode ~= "generated"
				or type(path) ~= "table"
				or #path < 2
				or #path > scaleState.maxStreamDepth + 1
				or path[1] ~= state.serviceName
			then
				return false, "disk delete path is not a generated path below its service"
			end
			for pathIndex, segment in ipairs(path) do
				if
					type(segment) ~= "string"
					or segment == ""
					or string.find(segment, "\0", 1, true)
					or (pathIndex == 1 and segment ~= state.serviceName)
				then
					return false, "disk delete path contains an invalid segment"
				end
			end
			for pathIndex in pairs(path) do
				if type(pathIndex) ~= "number" or pathIndex % 1 ~= 0 or pathIndex < 1 or pathIndex > #path then
					return false, "disk delete path must be a dense segment array"
				end
			end
			local deleteKey = table.concat(path, "\0")
			if state.deleteKeys[deleteKey] then
				return false, "disk delete stream repeated a generated path"
			end
			state.deleteKeys[deleteKey] = true
			state.deletes[#state.deletes + 1] = table.clone(path)
			if #state.deletes > scaleState.maxStreamNodes then
				return false, string.format("disk delete stream exceeds %d paths", scaleState.maxStreamNodes)
			end
			if deleteIndex % scaleState.deleteChunkRecords == 0 then
				task.wait()
				if not shouldContinue() then
					return false, mutationGuard.reason or "disk delete validation cancelled"
				end
			end
		end
		return true, nil
	end

	local function buildSelectiveDeletePlan(state, service): (any?, string?)
		if #state.deletes == 0 then
			return {
				groups = {},
				targetCount = 0,
			}, nil
		end

		-- Resolve every generated ordinal against the same unchanged Studio
		-- projection. Deleting as paths are resolved would renumber duplicate
		-- siblings (A [2] becomes A [1]) and can miss or delete the wrong target.
		-- Cache each parent's projection once so a 25k-wide selected delete is
		-- linear after one sibling sort instead of rebuilding it 25k times.
		local resolveContext = snapshotApplyState.newContext({
			shouldContinue = shouldContinue,
			indexAvoidSyncCarriers = true,
		})
		local siblingIndexes = {}
		local resolvedTargets = {}
		local entries = {}
		local groups = {}
		local groupsByParent = {}
		local ok, err = pcall(function()
			for _, deletePath in ipairs(state.deletes) do
				snapshotApplyState.checkpoint(resolveContext)
				local target = service
				for pathIndex = 2, #deletePath do
					local siblings = siblingIndexes[target]
					if not siblings then
						siblings = snapshotApplyState.buildPhysicalSiblingIndex(target, resolveContext)
						siblingIndexes[target] = siblings
					end
					target = siblings.byLookupSegment[deletePath[pathIndex]]
					if not target then
						break
					end
				end
				-- A target may already be absent by the time the authorized
				-- choice is redeemed. Preserve the old idempotent no-op behavior.
				if target then
					if target == service then
						error("selective delete resolved to its service root", 0)
					end
					if resolvedTargets[target] then
						error("selective delete paths resolve to the same Studio target", 0)
					end
					resolvedTargets[target] = true
					entries[#entries + 1] = {
						target = target,
					}
				end
			end

			-- Reject ancestor/descendant target pairs before any live mutation.
			-- A parent prune already owns its selected subtree; accepting both
			-- paths makes the result order-dependent and invalidates child groups.
			for _, entry in ipairs(entries) do
				local ancestor = entry.target.Parent
				while ancestor and ancestor ~= service do
					if resolvedTargets[ancestor] then
						error("selective delete targets overlap by ancestry", 0)
					end
					ancestor = ancestor.Parent
				end
				if ancestor ~= service then
					error("selective delete target escaped its service", 0)
				end
			end

			for _, entry in ipairs(entries) do
				local parent = entry.target.Parent
				if not parent then
					error("selective delete target lost its parent during planning", 0)
				end
				local group = groupsByParent[parent]
				if not group then
					group = {
						parent = parent,
						targets = {},
					}
					groupsByParent[parent] = group
					groups[#groups + 1] = group
				end
				group.targets[entry.target] = true
			end
		end)
		if not ok then
			return nil, tostring(err)
		end
		if not shouldContinue() then
			return nil, mutationGuard.reason or "Studio changed during selective delete planning"
		end
		return {
			groups = groups,
			targetCount = #entries,
		}, nil
	end

	local function cancelRecording(recordingId)
		local cancelled, cancelErr = pcall(function()
			snapshotApplyState.runInternalWrite(function()
				ChangeHistoryService:FinishRecording(recordingId, Enum.FinishRecordingOperation.Cancel)
			end, {
				recording = true,
			})
		end)
		if cancelled then
			return {
				ok = true,
				method = "cancel",
			}
		end
		-- Cancel normally performs the rollback itself. One Undo is the
		-- last-resort fallback for Studio channels where finishing Cancel
		-- errors. Preserve both errors so a double failure can halt sync
		-- without falsely claiming the all-service mutation was rolled back.
		local undone, undoErr = pcall(function()
			ChangeHistoryService:Undo()
		end)
		return {
			ok = undone,
			method = if undone then "undo" else nil,
			cancelError = tostring(cancelErr),
			undoError = if undone then nil else tostring(undoErr),
		}
	end

	local function applyStagedService(state, service, deletePlan)
		if not shouldContinue() then
			error(mutationGuard.reason or "Studio changed before streamed commit", 0)
		end
		state.instances = { service }
		state.skipped = {}
		snapshotApplyState.diskPathCache[service] = { state.serviceName }
		local applyOpts = {
			strict = false,
			forcePrune = false,
			shouldContinue = shouldContinue,
			claimedInstances = {},
			indexAvoidSyncCarriers = true,
		}
		applyOpts._applyContext = snapshotApplyState.newContext(applyOpts)
		state.applyContext = applyOpts._applyContext

		for parentId = 0, #state.records - 1 do
			snapshotApplyState.checkpoint(state.applyContext)
			local children = state.childrenByParent[parentId + 1]
			if children and #children > 0 then
				if state.skipped[parentId + 1] then
					for _, record in ipairs(children) do
						state.skipped[tonumber(record.id) + 1] = true
					end
				else
					local parent = state.instances[parentId + 1]
					if not parent then
						error("streamed commit parent Instance is missing", 0)
					end
					local nodes = {}
					for _, record in ipairs(children) do
						nodes[#nodes + 1] = {
							class = record.class,
							name = record.name,
							properties = {},
							children = {},
							-- Exact fragments select existing identities; newly
							-- created Instances still use the Roblox name/class.
							diskFragment = record.diskFragment,
							diskFragmentIsDir = record.diskFragmentIsDir,
							_streamId = record.id,
						}
					end
					local assignments = snapshotApplyState.buildAssignments(parent, nodes, state.applyContext)
					for _, assignment in ipairs(assignments) do
						local inst, skipChildren =
							snapshotApplyState.prepareInstance(parent, assignment, applyOpts, state.applyContext)
						local id = tonumber(assignment.node._streamId)
						state.instances[id + 1] = inst
						state.skipped[id + 1] = skipChildren == true
					end
				end
			end
		end

		for _, id in ipairs(state.sourceIds) do
			snapshotApplyState.checkpoint(state.applyContext)
			if not state.skipped[id + 1] then
				local staged = state.stagedInstances[id + 1]
				local target = state.instances[id + 1]
				local source = readScriptSource(staged)
				if
					sha256Hex(source) ~= state.sourceDigests[id + 1]
					and sha256Hex(normalizeSourceForCompare(source)) ~= state.sourceNormalizedDigests[id + 1]
				then
					error("detached Source changed before commit", 0)
				end
				if not target or not target:IsA("LuaSourceContainer") or not writeScriptSource(target, source) then
					error("ScriptEditorService rejected streamed Source commit", 0)
				end
				local readback = readScriptSource(target)
				local rawReadbackMatches = sha256Hex(readback) == state.sourceDigests[id + 1]
				local normalizedReadbackMatches = sha256Hex(normalizeSourceForCompare(readback))
					== state.sourceNormalizedDigests[id + 1]
				if not rawReadbackMatches and not normalizedReadbackMatches then
					error("streamed Source readback SHA-256 did not match the wire contract", 0)
				end
			end
		end

		for _, group in ipairs(deletePlan.groups) do
			snapshotApplyState.checkpoint(state.applyContext)
			for target in pairs(group.targets) do
				if state.applyContext.claimedInstances[target] then
					error("selective delete overlaps a streamed disk structure target", 0)
				end
				if target.Parent ~= group.parent then
					error("selective delete target changed after immutable planning", 0)
				end
			end
			local kept = {}
			local deleteCount = 0
			for _, sibling in ipairs(group.parent:GetChildren()) do
				if not group.targets[sibling] then
					kept[sibling] = true
				elseif isAvoidSyncBlocked(sibling) then
					kept[sibling] = true
					log("AvoidSync: preserving authorized delete target " .. pathOf(sibling))
				else
					deleteCount += 1
				end
			end
			if deleteCount > 0 then
				-- One parent-level prune removes the entire immutable target
				-- set without renumbering duplicate siblings between paths.
				-- Unselected siblings and Studio-authoritative/AvoidSync
				-- descendants survive.
				snapshotApplyState.pruneUnkept(group.parent, kept, state.applyContext)
			end
		end

		if strict then
			-- Destructive pruning is the final operation, after every staged
			-- Source was length/hash validated and applied successfully.
			for parentId = #state.records - 1, 0, -1 do
				snapshotApplyState.checkpoint(state.applyContext)
				if not state.skipped[parentId + 1] then
					local parent = state.instances[parentId + 1]
					if parent then
						local kept = {}
						for _, record in ipairs(state.childrenByParent[parentId + 1] or {}) do
							local child = state.instances[tonumber(record.id) + 1]
							if child then
								kept[child] = true
							end
						end
						snapshotApplyState.pruneUnkept(parent, kept, state.applyContext)
					end
				end
			end
		end
		if not shouldContinue() then
			error(mutationGuard.reason or "Studio changed during streamed commit", 0)
		end
	end

	local function commitAllServices(states): (boolean, string?)
		if not shouldContinue() then
			return false, mutationGuard.reason or "Studio changed before streamed commit"
		end
		-- Resolve every service and every selective delete against one
		-- unchanged live generation before opening the single recording.
		local plans = {}
		for _, state in ipairs(states) do
			local serviceOk, service = pcall(function()
				return game:GetService(state.serviceName)
			end)
			if not serviceOk or not service then
				return false, "streamed snapshot service is unavailable"
			end
			local deletePlan, deletePlanErr = buildSelectiveDeletePlan(state, service)
			if not deletePlan then
				return false, "selective delete planning failed: " .. tostring(deletePlanErr)
			end
			plans[#plans + 1] = {
				state = state,
				service = service,
				deletePlan = deletePlan,
			}
		end
		if not shouldContinue() then
			return false, mutationGuard.reason or "Studio changed before streamed commit"
		end
		local label = "Ro Sync: streamed snapshot"
		local recordingOk, recordingId = pcall(function()
			return ChangeHistoryService:TryBeginRecording(label, label)
		end)
		if not recordingOk or recordingId == nil then
			return false, "Studio refused to begin all-service streamed snapshot recording"
		end
		local committed, commitErr = pcall(function()
			for _, plan in ipairs(plans) do
				applyStagedService(plan.state, plan.service, plan.deletePlan)
			end
			if not shouldContinue() then
				error(mutationGuard.reason or "Studio changed during all-service commit", 0)
			end
		end)
		lastRemoteApplyClock = os.clock()
		if committed then
			local finishOk, finishErr = pcall(function()
				snapshotApplyState.runInternalWrite(function()
					ChangeHistoryService:FinishRecording(recordingId, Enum.FinishRecordingOperation.Commit)
				end, {
					recording = true,
				})
			end)
			if not finishOk then
				committed = false
				commitErr = "change-history commit failed: " .. tostring(finishErr)
			end
		end
		if not committed then
			local rollback = cancelRecording(recordingId)
			for _, plan in ipairs(plans) do
				snapshotApplyState.clearDiskPathSubtree(plan.service)
			end
			if not rollback.ok then
				local fatalMessage = string.format(
					"FATAL: all-service streamed snapshot may be partially applied; "
						.. "ChangeHistory Cancel failed (%s) and fallback Undo failed (%s). "
						.. "Original commit error: %s",
					tostring(rollback.cancelError),
					tostring(rollback.undoError),
					tostring(commitErr)
				)
				reconnectState.stopTerminal(fatalMessage)
				return false, fatalMessage
			end
			local fallbackDetail = if rollback.method == "undo"
				then " after ChangeHistory Cancel failed (" .. tostring(rollback.cancelError) .. ")"
				else ""
			return false,
				string.format(
					"all-service streamed snapshot commit rolled back via %s%s: %s",
					tostring(rollback.method),
					fallbackDetail,
					tostring(commitErr)
				)
		end
		return true, nil
	end

	local startBody = {
		pluginProtocol = PLUGIN_PROTOCOL_VERSION,
		requestId = requestId,
		phase = "start",
		strict = strict,
		avoidSyncPaths = scaleState.collectAvoidSyncPaths(),
	}
	if selective then
		startBody.choiceId = selectionChoiceId
	end
	local response, startErr = postExact(startBody, "snapshot stream start")
	if not response then
		disposeGuard()
		log("snapshot stream start failed: " .. tostring(startErr))
		setPill("error", "snapshot failed")
		return false
	end
	streamId = response.streamId
	if type(streamId) ~= "string" or streamId == "" then
		disposeGuard()
		log("snapshot stream omitted its streamId")
		return false
	end

	local expectedServiceIndex = 1
	local expectedService = WATCHED_SERVICES[expectedServiceIndex]
	local expectedPhase = "diskPrepare"
	local expectedChunk = 0
	local allowDiskPrepareReady = false
	while true do
		local responseChunk = integer(response.chunkIndex)
		local exactCoordinate = response.service == expectedService
			and response.phase == expectedPhase
			and responseChunk == expectedChunk
		local diskPrepareReady = allowDiskPrepareReady
			and expectedPhase == "diskPrepare"
			and response.service == expectedService
			and response.phase == "structure"
			and responseChunk == 0
		if
			response.streamId ~= streamId
			or (not exactCoordinate and not diskPrepareReady)
			or type(response.finalChunk) ~= "boolean"
		then
			disposeGuard()
			log("snapshot stream response was stale or out of order")
			return false
		end
		if diskPrepareReady then
			expectedPhase = "structure"
			expectedChunk = 0
			allowDiskPrepareReady = false
		end
		if not shouldContinue() then
			disposeGuard()
			log("snapshot stream aborted: " .. tostring(mutationGuard.reason or "cancelled"))
			return false
		end
		local terminalPhase = if selective then "deletes" else "sources"
		local terminalResponse = expectedServiceIndex == #WATCHED_SERVICES
			and expectedPhase == terminalPhase
			and response.finalChunk == true
		if
			(terminalResponse and response.action ~= "complete") or (not terminalResponse and response.action ~= nil)
		then
			disposeGuard()
			log("snapshot stream response carried a malformed or premature terminal action")
			return false
		end

		if expectedPhase == "diskPrepare" then
			if
				response.finalChunk ~= false
				or response.records ~= nil
				or response.sources ~= nil
				or response.deletes ~= nil
			then
				disposeGuard()
				log("snapshot disk preparation response carried a payload or final marker")
				return false
			end
			expectedChunk += 1
		elseif expectedPhase == "structure" then
			if response.sources ~= nil or response.deletes ~= nil then
				disposeGuard()
				log("snapshot structure response mixed payload phases")
				return false
			end
			if expectedChunk == 0 then
				serviceState = {
					serviceName = expectedService,
					records = {},
					depths = {},
					observedChildren = {},
					preorderStack = {},
					deletes = {},
					deleteKeys = {},
					stagedSourceBytes = 0,
				}
				stagedServiceStates[#stagedServiceStates + 1] = serviceState
			end
			local valid, structureErr =
				validateStructureChunk(serviceState, response.records, response.finalChunk == true)
			if not valid then
				disposeGuard()
				log("snapshot structure rejected: " .. tostring(structureErr))
				return false
			end
			if response.finalChunk then
				local staged, stageErr = stageStructure(serviceState)
				if not staged then
					disposeGuard()
					log("snapshot structure staging failed: " .. tostring(stageErr))
					return false
				end
				expectedPhase = "sources"
				expectedChunk = 0
			else
				expectedChunk += 1
			end
		elseif expectedPhase == "sources" then
			if response.records ~= nil or response.deletes ~= nil then
				disposeGuard()
				log("snapshot Source response mixed payload phases")
				return false
			end
			local valid, sourceErr = acceptSources(serviceState, response.sources, response.finalChunk == true)
			if not valid then
				disposeGuard()
				log("snapshot Source rejected: " .. tostring(sourceErr))
				return false
			end
			if response.finalChunk then
				if selective then
					expectedPhase = "deletes"
					expectedChunk = 0
				else
					if terminalResponse then
						-- No live service is touched before the sole authenticated
						-- terminal response. One recording makes the entire pull
						-- atomic across every watched service.
						local committed, commitErr = commitAllServices(stagedServiceStates)
						if not committed then
							disposeGuard()
							log("snapshot all-service commit failed: " .. tostring(commitErr))
							return false
						end
						break
					end
					expectedServiceIndex += 1
					expectedService = WATCHED_SERVICES[expectedServiceIndex]
					expectedPhase = "diskPrepare"
					expectedChunk = 0
					serviceState = nil
				end
			else
				expectedChunk += 1
			end
		elseif expectedPhase == "deletes" and selective then
			if response.records ~= nil or response.sources ~= nil then
				disposeGuard()
				log("snapshot delete response mixed payload phases")
				return false
			end
			local valid, deleteErr = acceptDeletes(serviceState, response.deletes, response.finalChunk == true)
			if not valid then
				disposeGuard()
				log("snapshot delete response rejected: " .. tostring(deleteErr))
				return false
			end
			if response.finalChunk then
				if terminalResponse then
					local committed, commitErr = commitAllServices(stagedServiceStates)
					if not committed then
						disposeGuard()
						log("snapshot all-service commit failed: " .. tostring(commitErr))
						return false
					end
					break
				end
				expectedServiceIndex += 1
				expectedService = WATCHED_SERVICES[expectedServiceIndex]
				expectedPhase = "diskPrepare"
				expectedChunk = 0
				serviceState = nil
			else
				expectedChunk += 1
			end
		else
			disposeGuard()
			log("snapshot stream entered an unsupported payload phase")
			return false
		end

		local nextBody = {
			pluginProtocol = PLUGIN_PROTOCOL_VERSION,
			requestId = requestId,
			streamId = streamId,
			service = expectedService,
			phase = expectedPhase,
			chunkIndex = expectedChunk,
		}
		allowDiskPrepareReady = expectedPhase == "diskPrepare"
		local nextResponse, nextErr =
			postExact(nextBody, string.format("%s %s chunk %d", expectedService, expectedPhase, expectedChunk))
		if not nextResponse then
			disposeGuard()
			log("snapshot stream fetch failed: " .. tostring(nextErr))
			return false
		end
		response = nextResponse
	end

	disposeGuard()
	return shouldContinue()
end

local function doPullSelectedChoice(selectedCount: any, choiceId: string?): boolean
	if
		type(selectedCount) ~= "number"
		or selectedCount <= 0
		or selectedCount % 1 ~= 0
		or selectedCount > scaleState.maxStreamNodes
	then
		log("initial-compare → selective pull rejected an invalid selectedCount")
		return false
	end
	if type(choiceId) ~= "string" or choiceId == "" then
		log("initial-compare → selective pull rejected a missing choiceId")
		return false
	end
	log(string.format("initial-compare → selective pull: streaming %d server-authorized disk path(s)", selectedCount))
	return doPullPath(true, selectedCount, choiceId)
end

-- A daemon or Desktop broker may advertise the authorized project initializer.
-- Keep this state near the connection UI that consumes it so the plugin's very
-- large top-level chunk can reuse earlier compiler registers.
local projectInitCandidate: any = nil
local connectionAttempt = 0
local activeInitialChoiceId: any = nil

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
	reconnectState.pinnedProject = nil
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
	activeInitialChoiceId = choiceId
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
			if #(resp.Body or "") > PROTOCOL_STREAM_MAX_BYTES then
				log("decision response exceeded the 512 KiB protocol limit")
				return "malformed"
			end
			local decoded
			local decOk = pcall(function()
				decoded = HttpService:JSONDecode(resp.Body or "")
			end)
			if decOk and type(decoded) == "table" then
				if decoded.paths ~= nil then
					log("decision response rejected obsolete paths payload")
					return "malformed"
				elseif decoded.pending == true then
					if decoded.selective ~= nil or decoded.selectedCount ~= nil then
						log("pending decision response contained premature disk selection metadata")
						return "malformed"
					end
					-- keep polling
				elseif decoded.choice then
					local choice = tostring(decoded.choice)
					local hasSelective = decoded.selective ~= nil
					local hasSelectedCount = decoded.selectedCount ~= nil
					if choice == "disk" and decoded.selective == true then
						local selectedCount = decoded.selectedCount
						if
							type(selectedCount) ~= "number"
							or selectedCount <= 0
							or selectedCount % 1 ~= 0
							or selectedCount > scaleState.maxStreamNodes
						then
							log("decision response rejected invalid selective selectedCount")
							return "malformed"
						end
						return choice, true, selectedCount
					elseif choice == "disk" then
						if hasSelective or hasSelectedCount then
							log("decision response rejected malformed full-disk selection metadata")
							return "malformed"
						end
						return choice, false, nil
					elseif hasSelective or hasSelectedCount then
						log("decision response attached disk selection metadata to a non-disk choice")
						return "malformed"
					end
					return choice, false, nil
				end
			elseif decOk then
				log("decision response was not a JSON object")
				return "malformed"
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
	local scanGuard = nil

	local function disposeScanGuard()
		if scanGuard then
			scaleState.disconnectStudioScanGuard(scanGuard)
			scanGuard = nil
		end
	end

	local function restartForMutation(context)
		local reason = scanGuard and scanGuard.reason or "Studio changed"
		disposeScanGuard()
		if not reconnectState.desired or not busy or compareGeneration ~= wsGeneration then
			return
		end
		log(string.format("%s (%s); restarting initial compare", context, tostring(reason)))
		-- Back off between restarts. Another plugin (or an edit-time script)
		-- mutating a watched service faster than a full scan completes would
		-- otherwise spin this restart tightly and silently forever, starving
		-- Studio of the idle window the scan needs to finish.
		reconnectState.mutationRestarts = math.min((reconnectState.mutationRestarts or 0) + 1, 8)
		local waitSec = math.min(0.25 * 2 ^ (reconnectState.mutationRestarts - 1), 10)
		if reconnectState.mutationRestarts > 2 then
			setBanner(
				string.format(
					"Studio kept changing while Ro Sync compared the place. Retrying in %.1fs (%s).",
					waitSec,
					tostring(reason)
				)
			)
		end
		local restartGeneration = wsGeneration
		task.delay(waitSec, function()
			if not reconnectState.desired or not busy or restartGeneration ~= wsGeneration then
				return
			end
			runInitialCompare()
		end)
	end

	local function scanBecameDirty(context): boolean
		if scanGuard and scanGuard.dirty then
			restartForMutation(context)
			return true
		end
		return false
	end

	local function completeInitialCompare()
		if not reconnectState.desired or not busy or compareGeneration ~= wsGeneration then
			disposeScanGuard()
			return
		end
		if scanBecameDirty("Studio changed before hooks were ready") then
			return
		end
		reconnectState.comparedCapability = compareCapability
		scaleState.activeScanGuard = scanGuard
		if startHooksAndWs() then
			reconnectState.mutationRestarts = 0
			disposeScanGuard()
		else
			restartForMutation("Studio changed while hooks were being installed")
		end
		scaleState.activeScanGuard = nil
	end

	scanGuard = scaleState.beginStudioScanGuard()
	local statsStarted = os.clock()
	local studioStats, statsProgress = scaleState.collectStudioStatsWithGuard(scanGuard)
	if not reconnectState.desired or not busy or compareGeneration ~= wsGeneration then
		disposeScanGuard()
		return
	end
	if scanBecameDirty("Studio changed during stats scan") then
		return
	end
	log(
		string.format(
			"Studio stats: %d visited, %d synced path(s), %d script(s), %.2fs",
			statsProgress.visited,
			studioStats.instanceCount,
			studioStats.scriptCount,
			os.clock() - statsStarted
		)
	)
	-- Stats-first lets a brand-new empty project choose `push` without ever
	-- serializing every Source into one place-sized comparison body.
	log("initial-compare: checking project stats")
	local ok, resp = httpJson("/initial-compare", "POST", {
		studioStats = studioStats,
		studioSnapshot = {},
		pluginProtocol = PLUGIN_PROTOCOL_VERSION,
	}, nil, PROTOCOL_STREAM_MAX_BYTES)
	if not reconnectState.desired or not busy or compareGeneration ~= wsGeneration then
		disposeScanGuard()
		return
	end
	if not ok then
		disposeScanGuard()
		log("initial-compare err: " .. tostring(resp))
		reconnectState.retryInitialCompare(resp)
		return
	end
	if type(resp) ~= "table" or not resp.action then
		disposeScanGuard()
		-- A daemon scan failure (symlink, unreadable file, case collision, bad
		-- default.project.json) arrives as a well-formed {ok=false, error=...}
		-- body, not a protocol violation. Report its text — it names the exact
		-- path to fix, which "malformed response" threw away.
		local scanError = if type(resp) == "table"
				and type(resp.error) == "string"
				and resp.error ~= ""
			then tostring(resp.error)
			else nil
		if scanError then
			log("initial-compare rejected the project: " .. scanError:sub(1, 300))
			reconnectState.stopTerminal("Ro Sync could not read this project: " .. scanError)
		else
			log("initial-compare: malformed response")
			reconnectState.stopTerminal("daemon returned a malformed initial-compare response")
		end
		return
	end
	local comparedDiskIdentities = nil
	local comparedDiskIdentityServices = nil
	if tostring(resp.action) == "compare" then
		-- Strong keys: populated chunk-by-chunk during the streamed comparison
		-- and only consumed after the final service completes. Weak Instance
		-- keys can be collected mid-stream (see projectionInfo), which would
		-- fail identity seeding for a clean comparison. Scoped to this compare.
		comparedDiskIdentities = {}
		comparedDiskIdentityServices = {}
		if scanBecameDirty("Studio changed before streamed comparison") then
			return
		end
		local compareId = if type(resp.compareId) == "string" then resp.compareId else nil
		local advertisedServices = resp.services
		local validSession = compareId ~= nil
			and compareId ~= ""
			and type(advertisedServices) == "table"
			and #advertisedServices == #WATCHED_SERVICES
		for index, serviceName in ipairs(WATCHED_SERVICES) do
			if not validSession or advertisedServices[index] ~= serviceName then
				validSession = false
				break
			end
		end
		if not validSession or resp.nextService ~= advertisedServices[1] then
			disposeScanGuard()
			log("initial-compare: daemon returned an invalid streamed comparison session")
			reconnectState.retryInitialCompare("invalid streamed comparison session")
			return
		end

		if resp.phase ~= "structure" or tonumber(resp.nextChunk) ~= 0 then
			disposeScanGuard()
			reconnectState.retryInitialCompare("daemon did not start the flat structure stream")
			return
		end

		log("initial-compare: streaming bounded structure + script hashes")
		local snapshotStarted = os.clock()
		local streamedRecords = 0
		local function makeCompareBody(serviceName, phase, chunkIndex, finalChunk, records, hashes)
			return {
				studioStats = studioStats,
				studioSnapshot = {},
				pluginProtocol = PLUGIN_PROTOCOL_VERSION,
				compareId = compareId,
				service = serviceName,
				phase = phase,
				chunkIndex = chunkIndex,
				finalChunk = finalChunk,
				records = records or {},
				hashes = hashes or {},
			}
		end
		local function encodedCompareBodySize(requestBody): (number?, string?)
			local encodedOk, encoded = pcall(function()
				return HttpService:JSONEncode(requestBody)
			end)
			if not encodedOk then
				return nil, "comparison JSONEncode failed: " .. tostring(encoded)
			end
			return #encoded, nil
		end
		local terminalCompareFailure = nil
		local function retryOrStopStreamedCompare(reason)
			if terminalCompareFailure ~= nil then
				local code = tostring(terminalCompareFailure.code or "terminal-comparison-error")
				local message = tostring(terminalCompareFailure.reason or reason or "daemon rejected comparison")
				log(string.format("initial-compare: terminal %s — %s", code, message:sub(1, 200)))
				reconnectState.stopTerminal(message)
				return
			end
			reconnectState.retryInitialCompare(reason)
		end
		local function postCompareChunk(serviceName, phase, chunkIndex, finalChunk, records, hashes)
			local requestBody = makeCompareBody(serviceName, phase, chunkIndex, finalChunk, records, hashes)
			local bodyBytes, encodeErr = encodedCompareBodySize(requestBody)
			if not bodyBytes then
				return false, encodeErr
			end
			if bodyBytes > scaleState.maxStreamRequestBytes then
				return false,
					string.format(
						"comparison %s/%s/%d encodes to %d bytes; maximum is %d",
						serviceName,
						phase,
						chunkIndex,
						bodyBytes,
						scaleState.maxStreamRequestBytes
					)
			end
			local lastErr = nil
			for attempt = 1, 2 do
				local requestOk, nextResp =
					httpJson("/initial-compare", "POST", requestBody, true, PROTOCOL_STREAM_MAX_BYTES)
				if not reconnectState.desired or not busy or compareGeneration ~= wsGeneration then
					return false, "comparison cancelled"
				end
				if scanGuard.dirty then
					return false, "Studio changed during streamed comparison"
				end
				if requestOk and type(nextResp) == "table" and nextResp.ok ~= false and nextResp.action then
					resp = nextResp
					return true, nil
				end
				if requestOk and type(nextResp) == "table" then
					lastErr = tostring(nextResp.error or "daemon rejected comparison chunk")
					if nextResp.retryable == false then
						terminalCompareFailure = {
							code = tostring(nextResp.code or "terminal-comparison-error"),
							reason = lastErr,
						}
						break
					end
				else
					lastErr = tostring(nextResp)
				end
				if attempt < 2 then
					log(
						string.format(
							"  ↻ %s %s chunk %d — retrying (%s)",
							serviceName,
							phase,
							chunkIndex,
							lastErr:sub(1, 140)
						)
					)
					task.wait(0.15)
				end
			end
			return false, lastErr or "streamed comparison chunk failed"
		end

		for index, serviceName in ipairs(advertisedServices) do
			if
				type(resp) ~= "table"
				or tostring(resp.action) ~= "compare"
				or resp.compareId ~= compareId
				or resp.nextService ~= serviceName
				or resp.phase ~= "structure"
				or tonumber(resp.nextChunk) ~= 0
			then
				disposeScanGuard()
				log("initial-compare: streamed comparison response was stale or out of order")
				reconnectState.retryInitialCompare("stale or out-of-order streamed comparison")
				return
			end
			if scanBecameDirty("Studio changed during streamed comparison") then
				return
			end

			local serviceOk, service = pcall(function()
				return game:GetService(serviceName)
			end)
			if not serviceOk or not service then
				disposeScanGuard()
				reconnectState.retryInitialCompare("could not read Studio service " .. tostring(serviceName))
				return
			end
			local sourceInstances, streamErr, instanceById = scaleState.streamStudioServiceStructure(
				service,
				scanGuard,
				function(_suggestedChunkIndex, finalChunk, records)
					local recordIndex = 1
					while recordIndex <= #records do
						local chunkIndex = tonumber(resp.nextChunk)
						if
							tostring(resp.action) ~= "compare"
							or resp.compareId ~= compareId
							or resp.nextService ~= serviceName
							or resp.phase ~= "structure"
							or not chunkIndex
						then
							return false, "daemon advanced the structure stream out of order"
						end
						local take = math.min(scaleState.structureChunkRecords, #records - recordIndex + 1)
						local batch = nil
						local finishesInput = false
						while take >= 1 do
							local candidateBatch = {}
							for offset = 0, take - 1 do
								candidateBatch[#candidateBatch + 1] = records[recordIndex + offset]
							end
							finishesInput = recordIndex + take - 1 == #records
							local candidate = makeCompareBody(
								serviceName,
								"structure",
								chunkIndex,
								finalChunk and finishesInput,
								candidateBatch,
								{}
							)
							local bytes = encodedCompareBodySize(candidate)
							if bytes and bytes <= scaleState.maxStreamRequestBytes then
								batch = candidateBatch
								break
							end
							take = math.floor(take / 2)
						end
						if not batch then
							return false,
								"one Studio comparison structure record exceeds the bounded 512 KiB request limit"
						end
						local sendsFinalChunk = finalChunk and finishesInput
						local sent, sendErr =
							postCompareChunk(serviceName, "structure", chunkIndex, sendsFinalChunk, batch, {})
						if not sent then
							return false, sendErr
						end
						streamedRecords += #batch
						recordIndex += #batch
						if not sendsFinalChunk then
							if
								tostring(resp.action) ~= "compare"
								or resp.compareId ~= compareId
								or resp.nextService ~= serviceName
								or resp.phase ~= "structure"
								or tonumber(resp.nextChunk) ~= chunkIndex + 1
							then
								return false, "daemon did not acknowledge the exact comparison structure cursor"
							end
						end
					end
					return true, nil, tonumber(resp.nextChunk)
				end
			)
			if not sourceInstances then
				disposeScanGuard()
				log(
					string.format(
						"initial-compare: %s structure stream failed: %s",
						serviceName,
						tostring(streamErr):sub(1, 200)
					)
				)
				retryOrStopStreamedCompare(streamErr or "streamed comparison failed")
				return
			end
			if
				tostring(resp.action) ~= "compare"
				or resp.compareId ~= compareId
				or resp.nextService ~= serviceName
				or resp.phase ~= "diskPrepare"
				or tonumber(resp.nextChunk) ~= 0
			then
				disposeScanGuard()
				reconnectState.retryInitialCompare("daemon did not start bounded disk preparation")
				return
			end
			local preparePolls = 0
			while resp.phase == "diskPrepare" do
				local prepareChunk = tonumber(resp.nextChunk)
				if
					tostring(resp.action) ~= "compare"
					or resp.compareId ~= compareId
					or resp.nextService ~= serviceName
					or not prepareChunk
					or prepareChunk < 0
					or prepareChunk % 1 ~= 0
				then
					disposeScanGuard()
					reconnectState.retryInitialCompare("daemon advanced disk preparation out of order")
					return
				end
				local prepared, prepareErr = postCompareChunk(serviceName, "diskPrepare", prepareChunk, false, {}, {})
				if not prepared then
					disposeScanGuard()
					retryOrStopStreamedCompare(prepareErr or "disk preparation continuation failed")
					return
				end
				preparePolls += 1
				if resp.phase == "diskPrepare" then
					if
						tostring(resp.action) ~= "compare"
						or resp.compareId ~= compareId
						or resp.nextService ~= serviceName
						or tonumber(resp.nextChunk) ~= prepareChunk + 1
					then
						disposeScanGuard()
						reconnectState.retryInitialCompare(
							"daemon did not acknowledge the exact disk preparation cursor"
						)
						return
					end
					if preparePolls % 20 == 0 then
						log(
							string.format(
								"  … %s disk preparation (%d continuation ticks)",
								serviceName,
								preparePolls
							)
						)
					end
					task.wait(0.05)
				elseif
					tostring(resp.action) ~= "compare"
					or resp.compareId ~= compareId
					or resp.nextService ~= serviceName
					or resp.phase ~= "identities"
					or tonumber(resp.nextChunk) ~= 0
				then
					disposeScanGuard()
					reconnectState.retryInitialCompare("daemon disk preparation did not advance to disk identities")
					return
				end
			end
			local identityCount = tonumber(resp.identityCount)
			if
				resp.nextService ~= serviceName
				or resp.phase ~= "identities"
				or tonumber(resp.nextChunk) ~= 0
				or not identityCount
				or identityCount < 0
				or identityCount % 1 ~= 0
				or identityCount > scaleState.maxStreamNodes
				or type(instanceById) ~= "table"
			then
				disposeScanGuard()
				reconnectState.retryInitialCompare("daemon did not start a valid disk identity stream")
				return
			end
			local identitySeen = {}
			-- Strong keys: sibling-collision baseline for the whole identity
			-- stream; a weak entry collected mid-stream would skip detection.
			local fragmentOwners = {}
			local receivedIdentities = 0
			while true do
				local identityChunk = tonumber(resp.nextChunk)
				if not identityChunk or identityChunk < 0 or identityChunk % 1 ~= 0 then
					disposeScanGuard()
					reconnectState.retryInitialCompare("daemon advanced disk identities out of order")
					return
				end
				local identityOk, identityErr = postCompareChunk(
					serviceName,
					"identities",
					identityChunk,
					false,
					{},
					{}
				)
				if not identityOk then
					disposeScanGuard()
					retryOrStopStreamedCompare(identityErr or "disk identity stream failed")
					return
				end
				local identities = resp.identities
				local finalIdentityChunk = resp.finalChunk
				if
					tostring(resp.action) ~= "compare"
					or resp.compareId ~= compareId
					or resp.nextService ~= serviceName
					or resp.phase ~= "identities"
					or tonumber(resp.chunkIndex) ~= identityChunk
					or tonumber(resp.identityCount) ~= identityCount
					or type(identities) ~= "table"
					or #identities > scaleState.structureChunkRecords
					or type(finalIdentityChunk) ~= "boolean"
					or (#identities == 0 and not finalIdentityChunk)
				then
					disposeScanGuard()
					reconnectState.retryInitialCompare("daemon returned a malformed disk identity chunk")
					return
				end
				for _, identity in ipairs(identities) do
					local id = type(identity) == "table" and tonumber(identity.id) or nil
					local fragment = type(identity) == "table" and identity.diskFragment or nil
					-- NOT `and ... or nil`: diskFragmentIsDir is false for every
					-- leaf script, and `and false or nil` collapses to nil —
					-- which rejected the entire identity stream on any place
					-- containing one plain script file.
					local isDirectory = if type(identity) == "table" then identity.diskFragmentIsDir else nil
					local inst = id and id % 1 == 0 and id > 0 and instanceById[id + 1] or nil
					local info = inst and scanGuard.projectionInfo[inst] or nil
					-- Name the failing identity and condition: a bare "invalid
					-- disk identity" banner is undebuggable against a
					-- thousand-node stream.
					local invalidReason = nil
					if not id or id % 1 ~= 0 then
						invalidReason = "identity id is not an integer"
					elseif identitySeen[id] then
						invalidReason = "duplicate identity id"
					elseif not inst then
						invalidReason = "id is outside the streamed Studio structure"
					elseif not info then
						invalidReason = "target instance left the projection"
					elseif info.avoidSync == true or info.avoidSyncCarrier == true then
						invalidReason = "identity targets an AvoidSync subtree"
					elseif not PathHelpers.isPortableDiskFragment(fragment) then
						invalidReason = "disk fragment is not portable"
					elseif type(isDirectory) ~= "boolean" then
						invalidReason = "fragment shape flag is missing"
					else
						-- A script legitimately exists on disk in either leaf form
						-- (Foo.luau) or script-with-children directory form
						-- (Foo/init (Foo).luau). Deleting a script's last child
						-- leaves the directory form behind, so requiring the
						-- physical shape to equal the current logical projection
						-- rejected a divergence this very handshake exists to
						-- reconcile — and rejected it permanently, since the retry
						-- rescans identical state. Only a node that CANNOT be
						-- represented as a single file must be a directory.
						-- Only a Folder is physically impossible as a file. Both
						-- script forms are interchangeable on disk, in BOTH
						-- directions: disk keeps directory form after a script
						-- loses its last child, and disk holds leaf form when
						-- Studio gained a child while disconnected (or a git
						-- checkout rewrote the tree). Each is a divergence the
						-- following push/pull reconciles — not a protocol fault.
						if info.class == "Folder" and not isDirectory then
							invalidReason = string.format(
								"shape mismatch at %s: Studio projects a Folder but disk holds a file",
								pathOf(inst)
							)
						end
					end
					if invalidReason then
						disposeScanGuard()
						reconnectState.retryInitialCompare(
							string.format(
								"daemon returned an invalid disk identity (id %s): %s",
								tostring(id),
								invalidReason
							)
						)
						return
					end
					local parent = inst.Parent
					if not parent then
						disposeScanGuard()
						reconnectState.retryInitialCompare("disk identity target lost its parent")
						return
					end
					local owners = fragmentOwners[parent]
					if not owners then
						owners = {}
						fragmentOwners[parent] = owners
					end
					local fragmentKey = string.lower(fragment)
					if owners[fragmentKey] and owners[fragmentKey] ~= inst then
						disposeScanGuard()
						reconnectState.retryInitialCompare("daemon returned colliding sibling disk identities")
						return
					end
					owners[fragmentKey] = inst
					identitySeen[id] = true
					comparedDiskIdentities[inst] = {
						fragment = fragment,
						isDirectory = isDirectory,
					}
					receivedIdentities += 1
					if receivedIdentities > identityCount then
						disposeScanGuard()
						reconnectState.retryInitialCompare("daemon exceeded its declared disk identity count")
						return
					end
				end
				if finalIdentityChunk then
					if
						receivedIdentities ~= identityCount
						or resp.nextPhase ~= "hashes"
						or tonumber(resp.nextChunk) ~= 0
					then
						disposeScanGuard()
						reconnectState.retryInitialCompare("daemon ended an incomplete disk identity stream")
						return
					end
					break
				end
				if tonumber(resp.nextChunk) ~= identityChunk + 1 then
					disposeScanGuard()
					reconnectState.retryInitialCompare("daemon did not acknowledge the exact disk identity cursor")
					return
				end
			end
			comparedDiskIdentityServices[serviceName] = true
			-- The final identity page advances daemon state to hashes. Keep the
			-- local cursor explicit rather than asking for an unbounded receipt.
			resp = {
				action = "compare",
				compareId = compareId,
				nextService = serviceName,
				phase = "hashes",
				nextChunk = 0,
			}

			local sourceIndex = 1
			local hashChunkIndex = 0
			repeat
				local hashes = {}
				while sourceIndex <= #sourceInstances and #hashes < scaleState.compareHashChunkRecords do
					local sourceEntry = sourceInstances[sourceIndex]
					local sourceHash, sourceErr = scaleState.normalizedSourceHash(sourceEntry.inst)
					if sourceHash == nil then
						disposeScanGuard()
						reconnectState.retryInitialCompare(
							"Studio Source could not be read coherently: " .. tostring(sourceErr)
						)
						return
					end
					hashes[#hashes + 1] = {
						id = sourceEntry.id,
						sha256 = sourceHash,
					}
					sourceIndex += 1
					if sourceIndex % scaleState.compareHashChunkRecords == 0 then
						task.wait()
						if scanGuard.dirty then
							disposeScanGuard()
							reconnectState.retryInitialCompare("Studio changed while hashing scripts")
							return
						end
					end
				end
				local finalHashChunk = sourceIndex > #sourceInstances
				if
					resp.nextService ~= serviceName
					or resp.phase ~= "hashes"
					or tonumber(resp.nextChunk) ~= hashChunkIndex
				then
					disposeScanGuard()
					reconnectState.retryInitialCompare("daemon advanced the hash stream out of order")
					return
				end
				local hashOk, hashErr =
					postCompareChunk(serviceName, "hashes", hashChunkIndex, finalHashChunk, {}, hashes)
				if not hashOk then
					disposeScanGuard()
					retryOrStopStreamedCompare(hashErr or "script hash stream failed")
					return
				end
				hashChunkIndex += 1
			until sourceIndex > #sourceInstances

			local nextService = advertisedServices[index + 1]
			if nextService then
				if
					tostring(resp.action) ~= "compare"
					or resp.compareId ~= compareId
					or resp.nextService ~= nextService
					or resp.phase ~= "structure"
					or tonumber(resp.nextChunk) ~= 0
				then
					disposeScanGuard()
					log("initial-compare: daemon advanced the comparison session out of order")
					reconnectState.retryInitialCompare("out-of-order streamed comparison response")
					return
				end
			elseif tostring(resp.action) == "compare" or not resp.action then
				disposeScanGuard()
				log("initial-compare: daemon did not finalize the streamed comparison")
				reconnectState.retryInitialCompare("streamed comparison did not finalize")
				return
			end
		end
		log(
			string.format(
				"Studio comparison stream: %d record(s), %d service(s), %.2fs",
				streamedRecords,
				#advertisedServices,
				os.clock() - snapshotStarted
			)
		)
	end
	if scanBecameDirty("Studio changed during snapshot comparison") then
		return
	end
	local action = tostring(resp.action)
	if action == "in-sync" then
		-- Seed only daemon-authored physical identities from the exact disk
		-- snapshot that produced this clean comparison. Re-deriving duplicate
		-- filenames from mutable Studio ordering would make future live writes
		-- target an inferred sibling rather than the file that was compared.
		log("initial-compare: already in sync, skipping decision modal")
		for _, serviceName in ipairs(WATCHED_SERVICES) do
			if
				not comparedDiskIdentities
				or not comparedDiskIdentityServices
				or comparedDiskIdentityServices[serviceName] ~= true
			then
				disposeScanGuard()
				reconnectState.retryInitialCompare("clean comparison omitted daemon-authored disk identities")
				return
			end
		end
		for _, serviceName in ipairs(WATCHED_SERVICES) do
			local serviceOk, service = pcall(function()
				return game:GetService(serviceName)
			end)
			if serviceOk and service then
				local seeded, seedErr = pcall(function()
					snapshotApplyState.seedDiskPathsForService(service, comparedDiskIdentities)
				end)
				if not seeded then
					disposeScanGuard()
					reconnectState.retryInitialCompare(
						"daemon-authored disk identity installation failed: " .. tostring(seedErr)
					)
					return
				end
			end
		end
		if scanBecameDirty("Studio changed while seeding live disk identities") then
			return
		end
		completeInitialCompare()
	elseif action == "push" then
		-- Empty-disk bootstrap streams one service at a time.
		if doPushPath(nil, scanGuard) then
			completeInitialCompare()
		elseif scanBecameDirty("Studio changed during snapshot push") then
			return
		else
			disposeScanGuard()
			reconnectState.retryInitialCompare("snapshot push failed")
		end
	elseif action == "pull" then
		-- Disk application intentionally mutates Studio; stop the consistency
		-- guard before applying it so our own writes do not look like user edits.
		disposeScanGuard()
		if doPullPath() then
			-- The pull's own mutations are complete. Guard the time-sliced hook
			-- install so a user edit in an as-yet-unhooked service cannot slip
			-- through the handoff.
			scanGuard = scaleState.beginStudioScanGuard()
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
			local choice, selectiveDiskChoice, selectedDiskCount = waitForDecision(choiceId, gen)
			if activeInitialChoiceId == choiceId then
				activeInitialChoiceId = nil
			end
			if gen ~= decisionGeneration then
				disposeScanGuard()
				return
			end
			if choice == "malformed" then
				disposeScanGuard()
				log("decision: malformed daemon response")
				disconnect("malformed initial decision")
				return
			end
			if scanBecameDirty("Studio changed while the overwrite choice was open") then
				return
			end
			if choice == "disk" then
				disposeScanGuard()
				local pulled = if selectiveDiskChoice == true
					then doPullSelectedChoice(selectedDiskCount, tostring(choiceId))
					else doPullPath(true)
				if pulled then
					scanGuard = scaleState.beginStudioScanGuard()
					completeInitialCompare()
				else
					reconnectState.retryInitialCompare("strict snapshot pull failed")
				end
			elseif choice == "studio" then
				-- The guard keeps the tree coherent while the modal is open;
				-- rescan and push one service at a time without retaining the
				-- comparison Sources.
				if doPushPath(true, scanGuard, choiceId) then
					completeInitialCompare()
				elseif scanBecameDirty("Studio changed during strict snapshot push") then
					return
				else
					disposeScanGuard()
					reconnectState.retryInitialCompare("strict snapshot push failed")
				end
			elseif choice == "cancel" then
				disposeScanGuard()
				log("decision: cancel")
				disconnect("cancelled by user")
			elseif choice == "stale" then
				disposeScanGuard()
				log("decision: stale choice; restarting initial compare")
				busy = true
				setPill("connecting")
				setBanner(nil)
				runInitialCompare()
			else
				disposeScanGuard()
				log("decision: unknown/aborted")
				disconnect("decision aborted")
			end
		end)
	else
		disposeScanGuard()
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
	-- Single-attempt: a whole-range scan must fail fast while the shared
	-- HttpService budget is exhausted, not wait a minute per probed port.
	local url = string.format("http://127.0.0.1:%d", port)
	local ok, resp = httpRequestTo(url .. "/hello", "GET", nil, nil, true)
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
	local expectedGameId = tostring(game.GameId)
	if expectedGameId ~= "0" and tostring(hello.gameId or "") ~= expectedGameId then
		return false, "daemon hello belongs to a different GameId"
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
	local daemonProject = hello.project
	if type(daemonProject) ~= "string" or daemonProject == "" then
		return false, "daemon hello omitted its canonical project identity"
	end
	return true, nil, capability, daemonProject
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

reconnectState.discoverDaemon = function(gameId, quiet, includeInitializer, expectedProject, shouldContinue)
	-- Probe deterministically. Roblox throttles a burst of localhost
	-- RequestAsync calls, and those requests cannot be cancelled after Connect
	-- is cancelled. A sequential scan keeps at most one stale request alive and
	-- checks the connection generation before starting every next port.
	--
	-- A PlaceId match may select one project among several daemons for the same
	-- experience. During automatic recovery, first restrict candidates to the
	-- exact canonical project selected by the user; never guess by lowest port
	-- when multiple exact candidates remain.
	if not quiet then
		setPill("probing")
		log(string.format("probing ports %d..%d for GameId %s", PORT_RANGE_START, PORT_RANGE_END, gameId))
	end
	local result = {
		found = false,
		url = nil,
		port = nil,
		sawWrongGame = false,
		sawWrongProject = false,
		ambiguous = false,
		candidateCount = 0,
		initializer = nil,
		initializerError = nil,
		cancelled = false,
	}
	local expectedProjectIdentity = if type(expectedProject) == "string" and expectedProject ~= ""
		then expectedProject
		else nil
	-- Probe the range with bounded concurrency. Sequentially, the scan costs one refusal per
	-- closed port, which is free only where the OS refuses immediately. Windows
	-- does not: a closed loopback port there was measured taking ~2s to refuse,
	-- so walking this range plus the broker range one port at a time left the
	-- panel sitting on "scanning localhost" for over half a minute. A small worker
	-- window avoids that serial cost without launching an uncancellable request
	-- against every port when the user cancels the connection attempt.
	local function probeRange(firstPort, lastPort)
		local probes = {}
		local nextPort = firstPort
		local outstanding = 0
		local acceptingResults = true
		local function isCurrentAttempt()
			return type(shouldContinue) ~= "function" or shouldContinue()
		end
		local function launchAvailable()
			while acceptingResults and outstanding < DAEMON_PROBE_CONCURRENCY and nextPort <= lastPort do
				if not isCurrentAttempt() then
					result.cancelled = true
					acceptingResults = false
					break
				end
				local port = nextPort
				nextPort += 1
				outstanding += 1
				task.spawn(function()
					local matched, url, hello = probePort(port, gameId)
					if acceptingResults and isCurrentAttempt() then
						probes[port] = { matched = matched, url = url, hello = hello }
					end
					outstanding -= 1
				end)
			end
		end
		-- Wait for the in-flight probes. The deadline is a backstop only: it
		-- must outlast a slow-to-refuse port so a healthy scan is never cut
		-- short, while still guaranteeing the scan terminates if a probe never
		-- returns. Ports that miss it stay absent from `probes`, which callers
		-- already treat as "nothing answered there".
		local deadline = os.clock() + 15
		launchAvailable()
		while (nextPort <= lastPort or outstanding > 0) and os.clock() < deadline do
			if not isCurrentAttempt() then
				result.cancelled = true
				break
			end
			launchAvailable()
			if outstanding > 0 then
				task.wait()
			end
		end
		acceptingResults = false
		return probes
	end

	local probes = probeRange(PORT_RANGE_START, PORT_RANGE_END)
	local daemonInitializer = nil
	local gameMatches = {}

	for port = PORT_RANGE_START, PORT_RANGE_END do
		local probe = probes[port]
		if probe then
			local exactProject = expectedProjectIdentity ~= nil
				and type(probe.hello) == "table"
				and probe.hello.project == expectedProjectIdentity
			if probe.matched or (gameId == "0" and exactProject) then
				gameMatches[#gameMatches + 1] = {
					url = probe.url,
					port = port,
					hello = probe.hello,
				}
				if not probe.matched then
					result.sawWrongGame = true
				end
			elseif probe.hello ~= nil then
				result.sawWrongGame = true
			end
			if daemonInitializer == nil and probe.hello ~= nil then
				daemonInitializer = projectInitOffer(probe.url, port, probe.hello)
			end
		end
	end

	if expectedProjectIdentity ~= nil then
		local projectMatches = {}
		for _, candidate in ipairs(gameMatches) do
			if
				type(candidate.hello) == "table"
				and type(candidate.hello.project) == "string"
				and candidate.hello.project == expectedProjectIdentity
			then
				projectMatches[#projectMatches + 1] = candidate
			else
				result.sawWrongProject = true
			end
		end
		gameMatches = projectMatches
	end

	local currentPlaceId = tostring(game.PlaceId)
	local exactPlaceMatches = {}
	for _, candidate in ipairs(gameMatches) do
		local placeIds = candidate.hello and candidate.hello.placeIds
		if type(placeIds) == "table" then
			for _, placeId in ipairs(placeIds) do
				if tostring(placeId) == currentPlaceId then
					exactPlaceMatches[#exactPlaceMatches + 1] = candidate
					break
				end
			end
		end
	end
	local candidates = if #exactPlaceMatches > 0 then exactPlaceMatches else gameMatches
	result.candidateCount = #candidates
	if #candidates == 1 then
		result.found = true
		result.url = candidates[1].url
		result.port = candidates[1].port
	elseif #candidates > 1 then
		result.ambiguous = true
	end

	-- The Desktop broker is deliberately outside the project-daemon range. It
	-- exists even when there are no projects yet, so inspect it only after no
	-- matching GameId daemon was found. Prefer it over a wrong-game daemon's
	-- initializer because Desktop owns the configured projects root and daemon
	-- lifecycle handoff.
	if not result.found and not result.ambiguous and includeInitializer ~= false then
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
	local protocolOk, protocolError, daemonPluginCapability, daemonProject = validateDaemonProtocol(chosenUrl)
	if not protocolOk then
		reconnectState.desired = false
		local reason = tostring(protocolError)
		log("connect blocked: " .. reason)
		-- Only a genuine version skew should tell the user to update Ro Sync and
		-- reinstall. validateDaemonProtocol also rejects a daemon that is simply
		-- unreachable, or one serving a different GameId, and naming those a
		-- version mismatch sends people to reinstall a plugin that was never at
		-- fault -- while the real cause stays buried in the log.
		if reason:lower():find("protocol", 1, true) then
			setPill("error", "update Ro Sync")
			setBanner("Daemon/plugin versions are incompatible. Update Ro Sync and reinstall the Studio plugin.")
		elseif reason:lower():find("gameid", 1, true) then
			setPill("error", "wrong project")
			setBanner(
				string.format(
					"That daemon serves a different experience. Serve a project whose ro-sync.json GameId is %s, then connect again.",
					tostring(game.GameId)
				)
			)
		elseif reason:lower():find("hello failed", 1, true) then
			setPill("error", "no daemon")
			setBanner(
				"No Ro Sync daemon answered. Serve this project from Ro Sync Desktop, or run `rosync daemon start`, then connect again."
			)
		else
			setPill("error", "action required")
			setBanner("Ro Sync connection rejected: " .. reason)
		end
		busy = false
		return false
	end

	projectInitCandidate = nil
	reconnectState.desired = true
	reconnectState.pluginCapability = daemonPluginCapability
	reconnectState.comparedCapability = nil
	reconnectState.pinnedProject = daemonProject
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
		elseif result.ambiguous then
			busy = false
			setPill("error", "multiple projects")
			setBanner(
				"Multiple Ro Sync projects now match this place. Enter the intended daemon URL manually to finish connecting."
			)
			log("project-init: daemon discovery became ambiguous")
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
		local cancelledChoiceId = activeInitialChoiceId
		activeInitialChoiceId = nil
		reconnectState.desired = false
		reconnectState.compareRetryPending = false
		reconnectState.pluginCapability = nil
		reconnectState.comparedCapability = nil
		reconnectState.pinnedProject = nil
		busy = false
		wsGeneration += 1
		decisionGeneration += 1
		connectionAttempt += 1
		projectInitCandidate = nil
		setPill("idle")
		setBanner(nil)
		log("cancelled")
		if cancelledChoiceId ~= nil then
			local cancelledBaseUrl = baseUrl
			task.spawn(function()
				local ok, response = httpRequestTo(cancelledBaseUrl .. "/initial-choice", "POST", {
					choiceId = cancelledChoiceId,
					choice = "cancel",
				})
				if ok and response.Success then
					log("cancelled pending overwrite decision")
				else
					log("could not cancel pending overwrite decision; it will expire as stale")
				end
			end)
		end
		return
	end

	-- Every explicit Connect/Create Project action selects a new daemon identity.
	-- Automatic retries keep the pin; user intent is the only reset boundary.
	reconnectState.pinnedProject = nil

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
				-- `node` and `requestBody` leave scope at the end of this
				-- iteration, before the next service's Source tree is scanned.
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
			local discoveryDeadline = os.clock() + DAEMON_STARTUP_GRACE_SECONDS
			local result = nil
			local scanCount = 0
			repeat
				scanCount += 1
				result = reconnectState.discoverDaemon(gameId, scanCount > 1, nil, nil, function()
					return attempt == connectionAttempt and busy
				end)
				if attempt ~= connectionAttempt or not busy or result.cancelled then
					return
				end
				if result.found or result.ambiguous then
					break
				end
				local remaining = math.max(0, math.ceil(discoveryDeadline - os.clock()))
				if remaining <= 0 then
					break
				end
				setPill("daemon_waiting", remaining)
				if scanCount == 1 then
					setBanner("Waiting for Ro Sync Desktop to start this project's daemon. Cancel stops waiting.")
					log(
						string.format(
							"no matching daemon yet; waiting up to %d seconds for Desktop startup",
							DAEMON_STARTUP_GRACE_SECONDS
						)
					)
				end
				local retryAt = math.min(discoveryDeadline, os.clock() + DAEMON_DISCOVERY_RETRY_SECONDS)
				while attempt == connectionAttempt and busy and os.clock() < retryAt do
					setPill("daemon_waiting", math.max(0, math.ceil(discoveryDeadline - os.clock())))
					task.wait(0.1)
				end
			until os.clock() >= discoveryDeadline
			if attempt ~= connectionAttempt or not busy then
				return
			end
			if not result.found then
				if result.ambiguous then
					setPill("error", "multiple projects")
					setBanner(
						"Multiple Ro Sync projects match this place. Enter the intended daemon URL manually instead of choosing by port."
					)
					log(
						string.format(
							"daemon discovery is ambiguous: %d project(s) match GameId %s and PlaceId %s",
							tonumber(result.candidateCount) or 0,
							gameId,
							tostring(game.PlaceId)
						)
					)
				elseif result.initializer ~= nil then
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
