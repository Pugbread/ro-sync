--!nonstrict
-- Ro Sync — Roblox Studio plugin.
-- Two-way sync between Studio DataModel and a local Rust daemon over HTTP.

local HttpService = game:GetService("HttpService")
local RunService = game:GetService("RunService")
local Selection = game:GetService("Selection")
local ChangeHistoryService = game:GetService("ChangeHistoryService")
local ScriptEditorService: any = nil
do
	local ok, svc = pcall(function() return game:GetService("ScriptEditorService") end)
	if ok then ScriptEditorService = svc end
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

-- Identifier returned by `rosync version` / `ping`. Bump when breaking changes
-- to the remoteHandlers protocol ship.
local PLUGIN_VERSION_STRING = "1.0.0"
local PLUGIN_PROTOCOL_VERSION = 1

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
	if messageType == Enum.MessageType.MessageError then return "error" end
	if messageType == Enum.MessageType.MessageWarning then return "warn" end
	return "info"
end

local function logLevelRank(level)
	if level == "error" then return 2 end
	if level == "warn" then return 1 end
	return 0
end

local function appendLogEntry(message, messageType)
	local entry = {
		t = os.clock(),
		wall = os.time(),
		level = mapLogLevel(messageType),
		message = tostring(message),
		seq = logNextSeq,
	}
	logNextSeq += 1
	logRingHead += 1
	if logRingHead > LOG_RING_SIZE then logRingHead = 1 end
	logRing[logRingHead] = entry
	if logRingCount < LOG_RING_SIZE then logRingCount += 1 end
end

do
	local ok, err = pcall(function()
		local LogService = game:GetService("LogService")
		LogService.MessageOut:Connect(function(message, messageType)
			appendLogEntry(message, messageType)
		end)
		-- Seed the buffer with any history LogService has already accumulated
		-- so `rosync logs` works right after plugin reload.
		local okHist, history = pcall(function() return LogService:GetLogHistory() end)
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
	if logRingCount == 0 then return {} end
	local cutoffClock
	if typeof(sinceSeconds) == "number" and sinceSeconds >= 0 then
		cutoffClock = os.clock() - sinceSeconds
	end
	local seqCutoff = (typeof(sinceSeq) == "number") and sinceSeq or nil
	local levelCutoff = levelMin and logLevelRank(levelMin) or 0
	local start = logRingHead - logRingCount + 1
	if start < 1 then start += LOG_RING_SIZE end
	local collected = {}
	for i = 0, logRingCount - 1 do
		local idx = start + i
		if idx > LOG_RING_SIZE then idx -= LOG_RING_SIZE end
		local e = logRing[idx]
		if e then
			local pass = true
			if cutoffClock and e.t < cutoffClock then pass = false end
			if pass and seqCutoff and e.seq <= seqCutoff then pass = false end
			if pass and logLevelRank(e.level) < levelCutoff then pass = false end
			if pass then table.insert(collected, e) end
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
local toggleButton = toolbar:CreateButton("Ro Sync", "Open the Ro Sync panel", "rbxasset://textures/StudioSharedUI/publish.png")
toggleButton.ClickableWhenViewportHidden = true

local widgetInfo = DockWidgetPluginGuiInfo.new(
	Enum.InitialDockState.Right, false, false, 360, 560, 280, 360
)
local widget = plugin:CreateDockWidgetPluginGui("RoSyncPanel", widgetInfo)
widget.Title = "Ro Sync"
widget.Name = "RoSyncPanel"

toggleButton.Click:Connect(function()
	widget.Enabled = not widget.Enabled
end)

local root = Instance.new("Frame")
root.Size = UDim2.fromScale(1, 1)
root.BackgroundColor3 = Color3.fromRGB(40, 40, 40)
root.BorderSizePixel = 0
root.Parent = widget

local pad = Instance.new("UIPadding")
pad.PaddingTop = UDim.new(0, 8)
pad.PaddingBottom = UDim.new(0, 8)
pad.PaddingLeft = UDim.new(0, 8)
pad.PaddingRight = UDim.new(0, 8)
pad.Parent = root

local layout = Instance.new("UIListLayout")
layout.SortOrder = Enum.SortOrder.LayoutOrder
layout.Padding = UDim.new(0, 6)
layout.Parent = root

local urlLabel = Instance.new("TextLabel")
urlLabel.LayoutOrder = 1
urlLabel.Size = UDim2.new(1, 0, 0, 16)
urlLabel.BackgroundTransparency = 1
urlLabel.TextColor3 = Color3.fromRGB(200, 200, 200)
urlLabel.Font = Enum.Font.SourceSans
urlLabel.TextSize = 14
urlLabel.TextXAlignment = Enum.TextXAlignment.Left
urlLabel.Text = "Manual URL override (leave blank to auto-discover)"
urlLabel.Parent = root

local urlBox = Instance.new("TextBox")
urlBox.LayoutOrder = 2
urlBox.Size = UDim2.new(1, 0, 0, 28)
urlBox.BackgroundColor3 = Color3.fromRGB(25, 25, 25)
urlBox.BorderSizePixel = 0
urlBox.TextColor3 = Color3.fromRGB(235, 235, 235)
urlBox.Font = Enum.Font.Code
urlBox.TextSize = 14
urlBox.TextXAlignment = Enum.TextXAlignment.Left
urlBox.ClearTextOnFocus = false
urlBox.PlaceholderText = DEFAULT_URL
urlBox.Text = tostring(plugin:GetSetting(SETTING_MANUAL_URL) or "")
urlBox.Parent = root

local urlPad = Instance.new("UIPadding")
urlPad.PaddingLeft = UDim.new(0, 6)
urlPad.PaddingRight = UDim.new(0, 6)
urlPad.Parent = urlBox

local statusRow = Instance.new("Frame")
statusRow.LayoutOrder = 3
statusRow.Size = UDim2.new(1, 0, 0, 32)
statusRow.BackgroundTransparency = 1
statusRow.Parent = root

local connectBtn = Instance.new("TextButton")
connectBtn.Size = UDim2.new(0, 100, 1, 0)
connectBtn.Position = UDim2.fromScale(0, 0)
connectBtn.BackgroundColor3 = Color3.fromRGB(64, 128, 220)
connectBtn.BorderSizePixel = 0
connectBtn.TextColor3 = Color3.fromRGB(255, 255, 255)
connectBtn.Font = Enum.Font.SourceSansBold
connectBtn.TextSize = 14
connectBtn.Text = "Connect"
connectBtn.AutoButtonColor = true
connectBtn.Parent = statusRow

local btnCorner = Instance.new("UICorner")
btnCorner.CornerRadius = UDim.new(0, 4)
btnCorner.Parent = connectBtn

local pill = Instance.new("TextLabel")
pill.Size = UDim2.new(1, -110, 1, -6)
pill.Position = UDim2.new(0, 108, 0, 3)
pill.BackgroundColor3 = Color3.fromRGB(70, 70, 70)
pill.BorderSizePixel = 0
pill.TextColor3 = Color3.fromRGB(230, 230, 230)
pill.Font = Enum.Font.SourceSansBold
pill.TextSize = 13
pill.Text = "idle"
pill.TextTruncate = Enum.TextTruncate.AtEnd
pill.Parent = statusRow

local pillCorner = Instance.new("UICorner")
pillCorner.CornerRadius = UDim.new(1, 0)
pillCorner.Parent = pill

local pillPad = Instance.new("UIPadding")
pillPad.PaddingLeft = UDim.new(0, 10)
pillPad.PaddingRight = UDim.new(0, 10)
pillPad.Parent = pill

-- Banner for decision handshake
local banner = Instance.new("TextLabel")
banner.LayoutOrder = 4
banner.Size = UDim2.new(1, 0, 0, 28)
banner.BackgroundColor3 = Color3.fromRGB(200, 150, 50)
banner.BorderSizePixel = 0
banner.TextColor3 = Color3.fromRGB(30, 30, 30)
banner.Font = Enum.Font.SourceSansBold
banner.TextSize = 13
banner.Text = ""
banner.Visible = false
banner.Parent = root

local bannerCorner = Instance.new("UICorner")
bannerCorner.CornerRadius = UDim.new(0, 4)
bannerCorner.Parent = banner

local logLabel = Instance.new("TextLabel")
logLabel.LayoutOrder = 5
logLabel.Size = UDim2.new(1, 0, 0, 16)
logLabel.BackgroundTransparency = 1
logLabel.TextColor3 = Color3.fromRGB(200, 200, 200)
logLabel.Font = Enum.Font.SourceSans
logLabel.TextSize = 14
logLabel.TextXAlignment = Enum.TextXAlignment.Left
logLabel.Text = "Log"
logLabel.Parent = root

local logFrame = Instance.new("ScrollingFrame")
logFrame.LayoutOrder = 6
logFrame.Size = UDim2.new(1, 0, 1, -160)
logFrame.BackgroundColor3 = Color3.fromRGB(20, 20, 20)
logFrame.BorderSizePixel = 0
logFrame.ScrollBarThickness = 6
logFrame.CanvasSize = UDim2.fromScale(0, 0)
logFrame.AutomaticCanvasSize = Enum.AutomaticSize.Y
logFrame.Parent = root

local logLayout = Instance.new("UIListLayout")
logLayout.SortOrder = Enum.SortOrder.LayoutOrder
logLayout.Padding = UDim.new(0, 2)
logLayout.Parent = logFrame

local logPad = Instance.new("UIPadding")
logPad.PaddingTop = UDim.new(0, 6)
logPad.PaddingBottom = UDim.new(0, 6)
logPad.PaddingLeft = UDim.new(0, 8)
logPad.PaddingRight = UDim.new(0, 8)
logPad.Parent = logFrame

local logCount = 0
local function log(msg)
	logCount += 1
	local line = Instance.new("TextLabel")
	line.LayoutOrder = logCount
	line.Size = UDim2.new(1, 0, 0, 16)
	line.BackgroundTransparency = 1
	line.TextColor3 = Color3.fromRGB(210, 210, 210)
	line.Font = Enum.Font.Code
	line.TextSize = 12
	line.TextXAlignment = Enum.TextXAlignment.Left
	line.TextTruncate = Enum.TextTruncate.AtEnd
	line.Text = string.format("[%.1fs] %s", os.clock(), tostring(msg))
	line.Parent = logFrame
	local children = logFrame:GetChildren()
	if #children - 2 > MAX_LOG_LINES then
		for _, c in ipairs(children) do
			if c:IsA("TextLabel") and c.LayoutOrder <= logCount - MAX_LOG_LINES then
				c:Destroy()
			end
		end
	end
	logFrame.CanvasPosition = Vector2.new(0, math.huge)
end

local function setPill(state, detail)
	-- state: "idle" | "probing" | "connected" | "no_daemon" | "wrong_game" | "error" | "connecting" | "waiting"
	if state == "idle" then
		pill.Text = "idle"
		pill.BackgroundColor3 = Color3.fromRGB(70, 70, 70)
		connectBtn.Text = "Connect"
	elseif state == "probing" then
		pill.Text = "probing…"
		pill.BackgroundColor3 = Color3.fromRGB(200, 150, 50)
		connectBtn.Text = "Cancel"
	elseif state == "connecting" then
		pill.Text = "connecting…"
		pill.BackgroundColor3 = Color3.fromRGB(200, 150, 50)
		connectBtn.Text = "Cancel"
	elseif state == "waiting" then
		pill.Text = "waiting decision…"
		pill.BackgroundColor3 = Color3.fromRGB(200, 150, 50)
		connectBtn.Text = "Cancel"
	elseif state == "connected" then
		pill.Text = string.format("connected :%s", tostring(detail or "?"))
		pill.BackgroundColor3 = Color3.fromRGB(60, 160, 90)
		connectBtn.Text = "Disconnect"
	elseif state == "no_daemon" then
		pill.Text = string.format("No ro-sync daemon found for GameId %s", tostring(detail or "?"))
		pill.BackgroundColor3 = Color3.fromRGB(200, 70, 70)
		connectBtn.Text = "Connect"
	elseif state == "wrong_game" then
		pill.Text = "wrong game"
		pill.BackgroundColor3 = Color3.fromRGB(200, 70, 70)
		connectBtn.Text = "Connect"
	elseif state == "error" then
		pill.Text = string.format("error: %s", tostring(detail or "?"))
		pill.BackgroundColor3 = Color3.fromRGB(200, 70, 70)
		connectBtn.Text = "Connect"
	else
		pill.Text = tostring(state)
		pill.BackgroundColor3 = Color3.fromRGB(70, 70, 70)
	end
end

local function setBanner(text)
	if text and text ~= "" then
		banner.Text = text
		banner.Visible = true
	else
		banner.Visible = false
		banner.Text = ""
	end
end

--------------------------------------------------------------------------------
-- Unpublished-place modal (GameId == "0")
--------------------------------------------------------------------------------

local modalOverlay = Instance.new("Frame")
modalOverlay.Size = UDim2.fromScale(1, 1)
modalOverlay.BackgroundColor3 = Color3.fromRGB(0, 0, 0)
modalOverlay.BackgroundTransparency = 0.35
modalOverlay.BorderSizePixel = 0
modalOverlay.Visible = false
modalOverlay.ZIndex = 50
modalOverlay.Parent = widget

local modalCard = Instance.new("Frame")
modalCard.Size = UDim2.new(1, -40, 0, 200)
modalCard.Position = UDim2.new(0, 20, 0.5, -100)
modalCard.BackgroundColor3 = Color3.fromRGB(40, 40, 40)
modalCard.BorderSizePixel = 0
modalCard.ZIndex = 51
modalCard.Parent = modalOverlay

local modalCorner = Instance.new("UICorner")
modalCorner.CornerRadius = UDim.new(0, 6)
modalCorner.Parent = modalCard

local modalPad = Instance.new("UIPadding")
modalPad.PaddingTop = UDim.new(0, 12)
modalPad.PaddingBottom = UDim.new(0, 12)
modalPad.PaddingLeft = UDim.new(0, 14)
modalPad.PaddingRight = UDim.new(0, 14)
modalPad.Parent = modalCard

local modalLayout = Instance.new("UIListLayout")
modalLayout.SortOrder = Enum.SortOrder.LayoutOrder
modalLayout.Padding = UDim.new(0, 8)
modalLayout.Parent = modalCard

local modalTitle = Instance.new("TextLabel")
modalTitle.LayoutOrder = 1
modalTitle.Size = UDim2.new(1, 0, 0, 20)
modalTitle.BackgroundTransparency = 1
modalTitle.TextColor3 = Color3.fromRGB(240, 240, 240)
modalTitle.Font = Enum.Font.SourceSansBold
modalTitle.TextSize = 16
modalTitle.TextXAlignment = Enum.TextXAlignment.Left
modalTitle.Text = "Unpublished place"
modalTitle.ZIndex = 52
modalTitle.Parent = modalCard

local modalBody = Instance.new("TextLabel")
modalBody.LayoutOrder = 2
modalBody.Size = UDim2.new(1, 0, 0, 60)
modalBody.BackgroundTransparency = 1
modalBody.TextColor3 = Color3.fromRGB(220, 220, 220)
modalBody.Font = Enum.Font.SourceSans
modalBody.TextSize = 14
modalBody.TextXAlignment = Enum.TextXAlignment.Left
modalBody.TextYAlignment = Enum.TextYAlignment.Top
modalBody.TextWrapped = true
modalBody.Text = "GameId is 0. Publish your place first, or enter a manual daemon URL below."
modalBody.ZIndex = 52
modalBody.Parent = modalCard

local modalUrlBox = Instance.new("TextBox")
modalUrlBox.LayoutOrder = 3
modalUrlBox.Size = UDim2.new(1, 0, 0, 28)
modalUrlBox.BackgroundColor3 = Color3.fromRGB(25, 25, 25)
modalUrlBox.BorderSizePixel = 0
modalUrlBox.TextColor3 = Color3.fromRGB(235, 235, 235)
modalUrlBox.Font = Enum.Font.Code
modalUrlBox.TextSize = 14
modalUrlBox.TextXAlignment = Enum.TextXAlignment.Left
modalUrlBox.ClearTextOnFocus = false
modalUrlBox.PlaceholderText = DEFAULT_URL
modalUrlBox.Text = ""
modalUrlBox.ZIndex = 52
modalUrlBox.Parent = modalCard

local modalUrlPad = Instance.new("UIPadding")
modalUrlPad.PaddingLeft = UDim.new(0, 6)
modalUrlPad.PaddingRight = UDim.new(0, 6)
modalUrlPad.Parent = modalUrlBox

local modalBtnRow = Instance.new("Frame")
modalBtnRow.LayoutOrder = 4
modalBtnRow.Size = UDim2.new(1, 0, 0, 32)
modalBtnRow.BackgroundTransparency = 1
modalBtnRow.ZIndex = 52
modalBtnRow.Parent = modalCard

local modalCancel = Instance.new("TextButton")
modalCancel.Size = UDim2.new(0, 90, 1, 0)
modalCancel.Position = UDim2.new(1, -200, 0, 0)
modalCancel.BackgroundColor3 = Color3.fromRGB(70, 70, 70)
modalCancel.BorderSizePixel = 0
modalCancel.TextColor3 = Color3.fromRGB(240, 240, 240)
modalCancel.Font = Enum.Font.SourceSansBold
modalCancel.TextSize = 14
modalCancel.Text = "Cancel"
modalCancel.ZIndex = 53
modalCancel.Parent = modalBtnRow

local modalCancelCorner = Instance.new("UICorner")
modalCancelCorner.CornerRadius = UDim.new(0, 4)
modalCancelCorner.Parent = modalCancel

local modalOk = Instance.new("TextButton")
modalOk.Size = UDim2.new(0, 100, 1, 0)
modalOk.Position = UDim2.new(1, -100, 0, 0)
modalOk.BackgroundColor3 = Color3.fromRGB(64, 128, 220)
modalOk.BorderSizePixel = 0
modalOk.TextColor3 = Color3.fromRGB(255, 255, 255)
modalOk.Font = Enum.Font.SourceSansBold
modalOk.TextSize = 14
modalOk.Text = "Use URL"
modalOk.ZIndex = 53
modalOk.Parent = modalBtnRow

local modalOkCorner = Instance.new("UICorner")
modalOkCorner.CornerRadius = UDim.new(0, 4)
modalOkCorner.Parent = modalOk

local modalResolve: ((string?) -> ())? = nil

local function showUnpublishedModal(onResult: (string?) -> ())
	modalUrlBox.Text = tostring(plugin:GetSetting(SETTING_MANUAL_URL) or "")
	modalResolve = onResult
	modalOverlay.Visible = true
end

local function hideUnpublishedModal(result: string?)
	modalOverlay.Visible = false
	local cb = modalResolve
	modalResolve = nil
	if cb then cb(result) end
end

modalCancel.Activated:Connect(function() hideUnpublishedModal(nil) end)
modalOk.Activated:Connect(function()
	local txt = modalUrlBox.Text or ""
	txt = (txt:gsub("^%s+", ""):gsub("%s+$", ""))
	if txt == "" then return end
	hideUnpublishedModal(txt)
end)

--------------------------------------------------------------------------------
-- Serialization
--------------------------------------------------------------------------------

-- Classes we never serialize, push, or apply. Camera lives under Workspace and
-- mutates on every viewport movement in Studio (CFrame / Focus / FieldOfView),
-- which would spam the daemon with ops. The user's viewport state isn't data
-- they want synced to disk.
local SUPPRESS_CLASSES = {
	["RBXScriptSignal"] = true,
	["Camera"] = true,
	["Terrain"] = true,            -- huge, binary voxel data; skip entirely
	["PlayerScripts"] = true,      -- runtime-only, per-player
	["PackageLink"] = true,        -- Roblox package bookkeeping
}

local function safeGet(inst, prop)
	local ok, val = pcall(function() return inst[prop] end)
	if ok then return val end
	return nil
end

-- The four Roblox classes that round-trip to/from disk. Every other class is
-- Studio-authoritative via the read-only tree.json skeleton — we never
-- Instance.new them, never apply property ops to them, never push their
-- changes to disk.
local ALLOWED_CLASSES = {
	Folder = true,
	Script = true,
	LocalScript = true,
	ModuleScript = true,
}

local function readScriptSource(inst): string
	if ScriptEditorService then
		local ok, src = pcall(function()
			return ScriptEditorService:GetEditorSource(inst)
		end)
		if ok and type(src) == "string" then return src end
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
-- filtered out at every level of recursion so Parts / Models / MeshParts /
-- etc. never appear in daemon payloads — those live only in the read-only
-- tree.json skeleton.
local function serializeInstance(inst)
	local node = {
		class = inst.ClassName,
		name = inst.Name,
		properties = serializeProperties(inst),
		children = {},
	}
	for _, child in ipairs(inst:GetChildren()) do
		if ALLOWED_CLASSES[child.ClassName] and not SUPPRESS_CLASSES[child.ClassName] then
			table.insert(node.children, serializeInstance(child))
		end
	end
	return node
end

-- Skeleton walker: records EVERY class's shape (class + name + children) with
-- no property values. The daemon dumps this verbatim to tree.json so external
-- tools (and the widget) know the full Studio tree, but the daemon never
-- attempts to round-trip non-ALLOWED classes.
local function skeletonNode(inst)
	local node = {
		class = inst.ClassName,
		name = inst.Name,
		children = {},
	}
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
for _, name in ipairs(WATCHED_SERVICES) do WATCHED_SET[name] = true end

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

local function resolvePath(path)
	local cur = game
	for _, seg in ipairs(path) do
		local nxt
		if cur == game then
			local ok, svc = pcall(function() return game:GetService(seg) end)
			if ok and svc then
				nxt = svc
			else
				nxt = cur:FindFirstChild(seg)
			end
		else
			nxt = cur:FindFirstChild(seg)
		end
		if not nxt then return nil end
		cur = nxt
	end
	return cur
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

local function writeScriptSource(inst, newSource: string)
	if ScriptEditorService then
		local ok = pcall(function()
			ScriptEditorService:UpdateSourceAsync(inst, function(_old)
				return newSource
			end)
		end)
		if ok then return end
	end
	pcall(function() inst.Source = newSource end)
end

-- Source is the only property we round-trip. Scripts get rewritten via
-- ScriptEditorService so Studio's editor sees the new text; everything else
-- is silently dropped — non-script props are Studio-authoritative.
local function applyProperties(inst, props)
	if type(props) ~= "table" then return end
	if props.Source ~= nil and inst:IsA("LuaSourceContainer") then
		writeScriptSource(inst, tostring(props.Source or ""))
	end
end

-- Count descendants cheaply; short-circuits at `cap` to avoid walking huge trees.
local function countDescendants(inst, cap: number): number
	local n = 0
	for _, child in ipairs(inst:GetChildren()) do
		n += 1
		if n >= cap then return n end
		n += countDescendants(child, cap - n)
		if n >= cap then return n end
	end
	return n
end

local function pathOf(inst: Instance): string
	if inst == game then return "game" end
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

local function canSafelyDestroy(inst: Instance): (boolean, string)
	if inst == game then
		return false, "root DataModel"
	end
	if inst.Parent == game then
		return false, "top-level service"
	end
	local n = countDescendants(inst, MAX_SAFE_DESTROY_DESCENDANTS + 1)
	if n > MAX_SAFE_DESTROY_DESCENDANTS then
		return false, string.format("%d+ descendants", n)
	end
	return true, ""
end

local function applyNode(parent, node, opts)
	-- Hard safety gate: only the four classes ro-sync is designed to
	-- round-trip ever get Instance.new()'d from disk-sourced payloads.
	-- Receiving anything else is a daemon/schema bug — log and bail before we
	-- can destroy an existing Studio instance to replace it with a stub.
	if not ALLOWED_CLASSES[node.class] then
		log(string.format(
			"SAFETY: skipping non-synced class %s at %s/%s",
			tostring(node.class), pathOf(parent), tostring(node.name)))
		return nil
	end
	local existing = parent:FindFirstChild(node.name)
	local inst
	if existing and existing.ClassName == node.class then
		inst = existing
	else
		if existing then
			local safe, reason = canSafelyDestroy(existing)
			if not safe then
				log(string.format(
					"SAFETY: refusing to destroy %s (%s → %s) — %s. Keeping existing, skipping new.",
					pathOf(existing), existing.ClassName, tostring(node.class), reason))
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
			local ok, created = pcall(function() return Instance.new(node.class) end)
			if not ok then
				log("unknown class: " .. tostring(node.class))
				return nil
			end
			inst = created
			inst.Name = node.name
			inst.Parent = parent
		end
	end
	applyProperties(inst, node.properties)
	local wanted = {}
	for _, c in ipairs(node.children or {}) do
		wanted[c.name] = true
		applyNode(inst, c, opts)
	end
	if opts and opts.strict then
		for _, child in ipairs(inst:GetChildren()) do
			-- Strict pruning only removes ALLOWED-class children. Non-ALLOWED
			-- classes (Parts, Models, Lights, etc.) are Studio-authoritative
			-- and never appear in disk payloads — skipping them here prevents
			-- a disk-driven apply from wiping Studio-only state.
			if ALLOWED_CLASSES[child.ClassName] and not wanted[child.Name] then
				local safe, reason = canSafelyDestroy(child)
				if safe then
					child:Destroy()
				else
					log(string.format(
						"SAFETY: strict-mode destroy skipped for %s — %s",
						pathOf(child), reason))
				end
			end
		end
	end
	return inst
end

local function applyOp(op)
	local kind = op.op or op.type
	if kind == "set" or kind == "replace" then
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
		applyNode(parent, op.node, { strict = op.strict == true })
	elseif kind == "delete" or kind == "remove" then
		local target = resolvePath(op.path)
		if target and target ~= game then
			local safe, reason = canSafelyDestroy(target)
			if safe then
				target:Destroy()
			else
				log(string.format(
					"SAFETY: ignoring delete op for %s — %s. Rename or move it manually if this was intended.",
					pathOf(target), reason))
			end
		end
	elseif kind == "update" then
		local target = resolvePath(op.path)
		if not target then return end
		-- Post-scope-down: update ops carry at most a Source string for a
		-- script. Name changes arrive as separate `rename` ops; every other
		-- property is Studio-authoritative and must not be applied here.
		local props = op.properties
		if type(props) == "table" and props.Source ~= nil and target:IsA("LuaSourceContainer") then
			writeScriptSource(target, tostring(props.Source or ""))
		end
	elseif kind == "rename" then
		local target = resolvePath(op.path)
		if target and op.name then target.Name = op.name end
	elseif kind == "move" then
		local target = resolvePath(op.from)
		local newParent = resolvePath(op.to)
		if target and newParent then target.Parent = newParent end
	else
		log("unknown op: " .. tostring(kind))
	end
end

local function setWaypoint(label: string)
	pcall(function() ChangeHistoryService:SetWaypoint(label) end)
end

local function summarizeOps(ops): string
	if not ops or #ops == 0 then return "noop" end
	local counts: {[string]: number} = {}
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
	if not ops or #ops == 0 then return end
	setWaypoint("Ro Sync: before " .. summarizeOps(ops))
	lastRemoteApplyClock = os.clock()
	local ok, err = pcall(function()
		for _, op in ipairs(ops) do applyOp(op) end
	end)
	lastRemoteApplyClock = os.clock()
	if not ok then
		log("applyOps error: " .. tostring(err))
	end
	setWaypoint("Ro Sync: " .. summarizeOps(ops))
end

local function applySnapshot(snap)
	-- Apply a full snapshot payload returned by /snapshot.
	setWaypoint("Ro Sync: before snapshot")
	lastRemoteApplyClock = os.clock()
	local ok, err = pcall(function()
		if snap and snap.services then
			local strict = snap.strict == true
			for _, node in ipairs(snap.services) do
				local svc = game:GetService(node.name)
				if svc then
					-- ALLOWED whitelist rejects non-ALLOWED (top-level services
					-- aren't ALLOWED). Services already exist, so apply each
					-- child directly into the resolved service.
					local wanted = {}
					for _, child in ipairs(node.children or {}) do
						wanted[child.name] = true
						applyNode(svc, child, { strict = strict })
					end
					if strict then
						for _, existing in ipairs(svc:GetChildren()) do
							if ALLOWED_CLASSES[existing.ClassName] and not wanted[existing.Name] then
								local safe, reason = canSafelyDestroy(existing)
								if safe then
									existing:Destroy()
								else
									log(string.format(
										"SAFETY: strict-mode destroy skipped for %s — %s",
										pathOf(existing), reason))
								end
							end
						end
					end
				end
			end
		elseif snap and snap.ops then
			for _, op in ipairs(snap.ops) do applyOp(op) end
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

-- Forward-declared so wsLoop (below) can re-enter the initial-sync handshake
-- after a `lagged` message. The real function assignment happens further down.
local runInitialCompare: () -> ()

-- Strip NULs and replace invalid UTF-8 bytes with the Unicode replacement
-- char (EF BF BD). JSONEncode rejects either. We keep valid UTF-8 intact.
local function scrubString(s)
	if #s == 0 then return s end
	-- Fast path: no NUL and pure ASCII (< 0x80).
	if not s:find("[%z\128-\255]") then return s end
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
			if b >= 0xF0 then seqLen = 4
			elseif b >= 0xE0 then seqLen = 3
			elseif b >= 0xC0 then seqLen = 2
			else seqLen = 0 end
			if seqLen > 0 and i + seqLen - 1 <= len then
				local ok = true
				for j = 1, seqLen - 1 do
					local cb = s:byte(i + j)
					if cb < 0x80 or cb >= 0xC0 then ok = false break end
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
	if depth > 128 then return nil end
	local t = typeof(v)
	if t == "nil" or t == "boolean" then
		return v
	elseif t == "string" then
		return scrubString(v)
	elseif t == "number" then
		if v ~= v or v == math.huge or v == -math.huge then return nil end
		return v
	elseif t == "table" then
		local isArray = true
		for k in pairs(v) do
			if type(k) ~= "number" then isArray = false break end
		end
		if isArray then
			local out = {}
			for i, item in ipairs(v) do
				local cleaned = sanitizeJson(item, depth + 1)
				if cleaned ~= nil then out[#out + 1] = cleaned end
			end
			return out
		else
			local out = {}
			for k, item in pairs(v) do
				if type(k) == "string" then
					local cleaned = sanitizeJson(item, depth + 1)
					if cleaned ~= nil then out[scrubString(k)] = cleaned end
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
		local encOk, enc = pcall(function() return HttpService:JSONEncode(clean) end)
		if not encOk then
			return false, "JSONEncode failed: " .. tostring(enc)
		end
		req.Body = enc
	end
	local ok, resp = pcall(function()
		return HttpService:RequestAsync(req)
	end)
	if not ok then return false, tostring(resp) end
	return true, resp
end

local function httpRequest(path, method, body)
	return httpRequestTo(baseUrl .. path, method, body)
end

local function httpJson(path, method, body)
	local ok, resp = httpRequest(path, method, body)
	if not ok then return false, resp end
	if not resp.Success then
		return false, string.format("HTTP %d %s", resp.StatusCode, resp.StatusMessage or "")
	end
	if resp.Body == nil or resp.Body == "" then return true, nil end
	local decoded
	local decOk, err = pcall(function()
		decoded = HttpService:JSONDecode(resp.Body)
	end)
	if not decOk then return false, "decode: " .. tostring(err) end
	return true, decoded
end

--------------------------------------------------------------------------------
-- Pushing local changes (Studio -> FS)
--------------------------------------------------------------------------------

local pendingOps = {}
local pushPending = false

local function flushPush()
	if #pendingOps == 0 then return end
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
		if not pushPending then
			pushPending = true
			task.delay(0.25, function()
				pushPending = false
				flushPush()
			end)
		end
		return
	end
	local ops = pendingOps
	pendingOps = {}
	local msg = { type = "push", ops = ops }
	local clean = sanitizeJson(msg)
	local encOk, enc = pcall(function() return HttpService:JSONEncode(clean) end)
	if not encOk then
		log("push encode fail: " .. tostring(enc))
		return
	end
	local sendOk, sendErr = pcall(function() client:Send(enc) end)
	if not sendOk then
		log(string.format("push err: %s (%d op(s) dropped)", tostring(sendErr), #ops))
	end
end

local function queueOp(op)
	if isEchoing() then return end
	table.insert(pendingOps, op)
	if not pushPending then
		pushPending = true
		task.delay(0.1, function()
			pushPending = false
			flushPush()
		end)
	end
end

--------------------------------------------------------------------------------
-- Tree skeleton push (Studio → daemon tree.json)
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
		local ok, svc = pcall(function() return game:GetService(name) end)
		if ok and svc then
			table.insert(services, skeletonNode(svc))
		end
	end
	return services
end

local function pushTreeSkeleton()
	if not treeDirty then return end
	if not connected then return end
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

local function markTreeDirty()
	treeDirty = true
	if treePushPending then return end
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
local connections: {[Instance]: {RBXScriptConnection}} = {}
local serviceConnections: {RBXScriptConnection} = {}
-- Name at the time we first connected the instance. When Changed fires for
-- "Name", the instance already holds the new name and pathFromRoot would
-- reconstruct the new path — useless for telling the daemon *which* file to
-- rename. Tracking the prior name here lets us emit a `rename` op with the
-- correct old path + new name.
local lastKnownName: {[Instance]: string} = {}

-- Forward declarations so the event callbacks can refer to each other. These
-- are plain locals rather than `local function` so the later `function foo()`
-- sugar unambiguously rebinds the existing local in all Lua/Luau variants.
local onDescendantAdded
local onDescendantRemoving
local onChanged
local connectInstance
local connectTree
local disconnectInstance
local disconnectTree

onDescendantAdded = function(inst)
	if isEchoing() then return end
	if not isUnderWatched(inst) then return end
	if SUPPRESS_CLASSES[inst.ClassName] then return end
	-- Any Studio-side addition — synced OR Studio-authoritative — changes the
	-- skeleton. Mark dirty so pushTreeSkeleton picks it up on the next tick.
	markTreeDirty()
	-- Non-ALLOWED classes (Parts, Models, Lights, etc.) are Studio-
	-- authoritative: they live only in tree.json and never generate set/delete
	-- ops against the daemon.
	if not ALLOWED_CLASSES[inst.ClassName] then return end
	queueOp({
		op = "set",
		path = pathFromRoot(inst.Parent),
		node = serializeInstance(inst),
	})
end

onDescendantRemoving = function(inst)
	if isEchoing() then return end
	if not isUnderWatched(inst) then return end
	markTreeDirty()
	if not ALLOWED_CLASSES[inst.ClassName] then return end
	local path = pathFromRoot(inst)
	log(string.format("delete: %s", table.concat(path, "/")))
	queueOp({
		op = "delete",
		path = path,
	})
end

onChanged = function(inst, prop)
	if isEchoing() then return end
	if not isUnderWatched(inst) then return end
	-- Parent changes are observed via ChildAdded/ChildRemoved on the
	-- surrounding instances; emitting here would double-report moves.
	if prop == "Parent" then return end
	if prop == "Name" then
		-- Renames affect every tree.json consumer, not just the synced files.
		markTreeDirty()
		-- Non-ALLOWED instances don't exist on disk, so there's no file to
		-- rename — the tree.json push above is all the daemon needs.
		if not ALLOWED_CLASSES[inst.ClassName] then
			lastKnownName[inst] = inst.Name
			return
		end
		-- Dedicated rename op: daemon needs the OLD path to locate the file
		-- it has to rename. pathFromRoot(inst) already reflects the new name
		-- because Changed fires after the assignment, so we reconstruct the
		-- old path from the cached last-known name.
		local oldName = lastKnownName[inst] or inst.Name
		lastKnownName[inst] = inst.Name
		local parentPath = inst.Parent and pathFromRoot(inst.Parent) or {}
		local oldPath = {}
		for i, s in ipairs(parentPath) do oldPath[i] = s end
		table.insert(oldPath, oldName)
		log(string.format("rename: %s → %s (path depth %d)", oldName, inst.Name, #oldPath))
		queueOp({
			op = "rename",
			path = oldPath,
			name = inst.Name,
		})
		return
	end
	if prop == "Source" and inst:IsA("LuaSourceContainer") then
		markTreeDirty()
		queueOp({
			op = "update",
			path = pathFromRoot(inst),
			properties = { Source = readScriptSource(inst) },
		})
		return
	end
	-- Every other property is Studio-authoritative. No op, no tree push —
	-- tree.json shape depends on class+name+children, none of which change
	-- on a plain property edit.
end

disconnectInstance = function(inst)
	local bucket = connections[inst]
	if not bucket then return end
	for _, c in ipairs(bucket) do
		pcall(function() c:Disconnect() end)
	end
	connections[inst] = nil
	lastKnownName[inst] = nil
end

disconnectTree = function(inst)
	for _, child in ipairs(inst:GetChildren()) do
		disconnectTree(child)
	end
	disconnectInstance(inst)
end

connectInstance = function(inst)
	if connections[inst] then return end
	if SUPPRESS_CLASSES[inst.ClassName] then return end
	local bucket: {RBXScriptConnection} = {}
	connections[inst] = bucket
	lastKnownName[inst] = inst.Name
	table.insert(bucket, inst.Changed:Connect(function(prop)
		onChanged(inst, prop)
	end))
	table.insert(bucket, inst.ChildAdded:Connect(function(child)
		onDescendantAdded(child)
		connectTree(child)
	end))
	table.insert(bucket, inst.ChildRemoved:Connect(function(child)
		onDescendantRemoving(child)
		disconnectTree(child)
	end))
end

connectTree = function(inst)
	connectInstance(inst)
	for _, child in ipairs(inst:GetChildren()) do
		connectTree(child)
	end
end

local function disconnectHooks()
	for _, c in ipairs(serviceConnections) do
		pcall(function() c:Disconnect() end)
	end
	serviceConnections = {}
	for inst, _ in pairs(connections) do
		disconnectInstance(inst)
	end
	connections = {}
end

local function hookService(svc)
	-- The service itself is a root — connect its descendants individually so
	-- each can be torn down on removal. We skip the service's own Changed
	-- signal (its properties aren't part of the sync surface).
	table.insert(serviceConnections, svc.ChildAdded:Connect(function(child)
		onDescendantAdded(child)
		connectTree(child)
	end))
	table.insert(serviceConnections, svc.ChildRemoved:Connect(function(child)
		onDescendantRemoving(child)
		disconnectTree(child)
	end))
	for _, child in ipairs(svc:GetChildren()) do
		connectTree(child)
	end
end

local function installHooks()
	disconnectHooks()
	for _, name in ipairs(WATCHED_SERVICES) do
		local ok, svc = pcall(function() return game:GetService(name) end)
		if ok and svc then
			hookService(svc)
		else
			log("skip service: " .. name)
		end
	end
end

--------------------------------------------------------------------------------
-- Studio stats (for initial-compare)
--------------------------------------------------------------------------------

-- Initial-compare stats: daemon compares disk counts vs. Studio counts to
-- decide push-vs-pull. Only the four ALLOWED classes live on disk, so that's
-- all we count — otherwise a Studio-full-of-Parts place would always beat a
-- scripts-only disk on instanceCount and force a spurious push.
local function collectStudioStats()
	local scriptCount = 0
	local instanceCount = 0
	for _, name in ipairs(WATCHED_SERVICES) do
		local ok, svc = pcall(function() return game:GetService(name) end)
		if ok and svc then
			for _, desc in ipairs(svc:GetDescendants()) do
				if ALLOWED_CLASSES[desc.ClassName] then
					instanceCount += 1
					if desc:IsA("LuaSourceContainer") then
						scriptCount += 1
					end
				end
			end
		end
	end
	return { scriptCount = scriptCount, instanceCount = instanceCount }
end

-- Snapshot for /push bootstrap. serializeInstance already filters recursion
-- to ALLOWED, but if a service has *only* non-ALLOWED descendants it would
-- still emit a service node with an empty children[]. That's fine — the
-- daemon only materializes directories for services that actually contain
-- synced files, and bootstrapping an empty service is a no-op.
local function collectSnapshot()
	local services = {}
	for _, name in ipairs(WATCHED_SERVICES) do
		local ok, svc = pcall(function() return game:GetService(name) end)
		if ok and svc then
			table.insert(services, serializeInstance(svc))
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
	if not client then return false end
	local clean = sanitizeJson(msg)
	local encOk, enc = pcall(function() return HttpService:JSONEncode(clean) end)
	if not encOk then
		log("ws send encode fail: " .. tostring(enc))
		return false
	end
	local ok, err = pcall(function() client:Send(enc) end)
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
		local ok, s = pcall(function() return buffer.tostring(payload) end)
		if ok then text = s end
	else
		return nil
	end
	if not text or text == "" then return nil end
	local decoded
	local decOk, err = pcall(function()
		decoded = HttpService:JSONDecode(text)
	end)
	if not decOk then
		log("ws decode err: " .. tostring(err))
		return nil
	end
	if type(decoded) ~= "table" then return nil end
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

local function splitRemotePath(p: string): {string}
	local segs = {}
	if p == nil or p == "" or p == "/" then return segs end
	for seg in string.gmatch(tostring(p), "[^/]+") do
		table.insert(segs, seg)
	end
	return segs
end

local function resolveRemotePath(p: string): Instance?
	local segs = splitRemotePath(p)
	if #segs == 0 then return game end
	return resolvePath(segs)
end

local function instancePathString(inst: Instance): string
	if inst == nil or inst == game then return "" end
	local parts = {}
	local cur = inst
	while cur and cur ~= game do
		table.insert(parts, 1, cur.Name)
		cur = cur.Parent
	end
	return table.concat(parts, "/")
end

local encodeRemoteValue
function encodeRemoteValue(v, depth)
	depth = depth or 0
	if depth > 64 then return nil end
	local t = typeof(v)
	if t == "nil" or t == "boolean" or t == "string" then
		return v
	elseif t == "number" then
		if v ~= v or v == math.huge or v == -math.huge then return nil end
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
		return { __type = "UDim2",
			x = { scale = v.X.Scale, offset = v.X.Offset },
			y = { scale = v.Y.Scale, offset = v.Y.Offset } }
	elseif t == "EnumItem" then
		return { __type = "EnumItem",
			enum = tostring(v.EnumType), name = v.Name, value = v.Value }
	elseif t == "BrickColor" then
		return { __type = "BrickColor", name = v.Name }
	elseif t == "NumberRange" then
		return { __type = "NumberRange", min = v.Min, max = v.Max }
	elseif t == "Instance" then
		return { __type = "Instance", path = instancePathString(v), class = v.ClassName }
	elseif t == "table" then
		local isArray = true
		for k in pairs(v) do
			if type(k) ~= "number" then isArray = false break end
		end
		if isArray then
			local out = {}
			for i, item in ipairs(v) do
				out[i] = encodeRemoteValue(item, depth + 1)
			end
			return out
		else
			local out = {}
			for k, item in pairs(v) do
				if type(k) == "string" then
					out[k] = encodeRemoteValue(item, depth + 1)
				end
			end
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
	if t ~= "table" then return v end
	local tag = v.__type
	if tag == nil then return v end -- untagged plain table passes through
	if tag == "Vector2" then
		return Vector2.new(tonumber(v.x) or 0, tonumber(v.y) or 0)
	elseif tag == "Vector3" then
		return Vector3.new(tonumber(v.x) or 0, tonumber(v.y) or 0, tonumber(v.z) or 0)
	elseif tag == "CFrame" then
		local c = v.components or {}
		-- Accept either the 3-arg (pos only) or 12-arg full matrix form.
		if #c == 3 then return CFrame.new(c[1], c[2], c[3]) end
		if #c == 12 then
			return CFrame.new(c[1], c[2], c[3], c[4], c[5], c[6], c[7], c[8], c[9], c[10], c[11], c[12])
		end
		error("CFrame components must have length 3 or 12 (got " .. #c .. ")")
	elseif tag == "Color3" then
		return Color3.new(tonumber(v.r) or 0, tonumber(v.g) or 0, tonumber(v.b) or 0)
	elseif tag == "UDim" then
		return UDim.new(tonumber(v.scale) or 0, tonumber(v.offset) or 0)
	elseif tag == "UDim2" then
		local x = v.x or {}; local y = v.y or {}
		return UDim2.new(tonumber(x.scale) or 0, tonumber(x.offset) or 0,
			tonumber(y.scale) or 0, tonumber(y.offset) or 0)
	elseif tag == "EnumItem" then
		local enumName = tostring(v.enum or "")
		local enumType = Enum[enumName]
		if not enumType then error("unknown Enum: " .. enumName) end
		local ok, item = pcall(function() return enumType[tostring(v.name or "")] end)
		if not ok or item == nil then
			error("unknown EnumItem: " .. enumName .. "." .. tostring(v.name))
		end
		return item
	elseif tag == "BrickColor" then
		return BrickColor.new(tostring(v.name or "Medium stone grey"))
	elseif tag == "NumberRange" then
		return NumberRange.new(tonumber(v.min) or 0, tonumber(v.max) or 0)
	elseif tag == "Instance" then
		local inst = resolveRemotePath(tostring(v.path or ""))
		if not inst then error("instance not found: " .. tostring(v.path)) end
		return inst
	end
	return v
end

local remoteHandlers = {}

-- Broad read-only property sweep. pcall'd per-property so unsupported names on
-- this class just get skipped. Not exhaustive — covers what an LLM is likely to
-- ask about when inspecting an instance. `rosync get --prop <name>` still
-- reaches any property directly.
local COMMON_PROPS = {
	"Name", "ClassName",
	-- BasePart
	"Position", "Size", "Orientation", "Rotation", "CFrame", "Anchored",
	"CanCollide", "CanTouch", "CanQuery", "CastShadow", "Massless",
	"Transparency", "Reflectance", "Material", "Color", "BrickColor",
	"TopSurface", "BottomSurface", "CollisionGroup", "Shape",
	-- MeshPart / SpecialMesh
	"MeshId", "TextureID", "MeshScale", "Scale", "MeshType", "VertexColor",
	-- SpawnLocation
	"Enabled", "Neutral", "AllowTeamChangeOnTouch", "Duration", "TeamColor",
	-- Script / LocalScript / ModuleScript
	"Source", "Disabled", "RunContext",
	-- GuiObject
	"AnchorPoint", "BackgroundColor3", "BackgroundTransparency",
	"BorderColor3", "BorderSizePixel", "ZIndex", "Visible", "LayoutOrder",
	"Active", "AutomaticSize",
	-- TextLabel / TextButton / TextBox
	"Text", "TextColor3", "TextSize", "TextScaled", "Font", "FontFace",
	"TextXAlignment", "TextYAlignment", "RichText", "TextWrapped",
	"TextStrokeColor3", "TextStrokeTransparency", "LineHeight",
	-- ImageLabel / ImageButton
	"Image", "ImageColor3", "ImageTransparency", "ScaleType", "ImageRectSize",
	"ImageRectOffset", "SliceCenter", "SliceScale",
	-- Attachment
	"Axis", "SecondaryAxis", "WorldPosition", "WorldCFrame",
	-- Beam / Trail
	"Attachment0", "Attachment1", "Width0", "Width1", "Texture",
	"TextureLength", "TextureSpeed", "TextureMode", "LightEmission",
	"LightInfluence", "FaceCamera", "Segments",
	-- ParticleEmitter
	"Rate", "Lifetime", "Speed", "SpreadAngle", "Rotation", "RotSpeed",
	"EmissionDirection", "Acceleration", "Drag",
	-- Camera
	"Focus", "FieldOfView", "CameraType", "CameraSubject", "HeadScale",
	-- Light
	"Brightness", "Range", "Shadows", "Angle", "Face",
	-- Sound
	"SoundId", "Volume", "Pitch", "PlaybackSpeed", "Looped", "Playing",
	"TimePosition", "PlayOnRemove", "RollOffMode", "RollOffMaxDistance",
	-- Decal / Texture
	"Face", "StudsPerTileU", "StudsPerTileV",
	-- Instance meta
	"Archivable", "Parent",
}

-- Extra property names used when inst:GetProperties() isn't exposed by this
-- Studio build. Combined with COMMON_PROPS inside buildInspectPropsList —
-- pcall-guarded, so unknown props on the target class are skipped.
local BROAD_INSPECT_PROPS = {
	-- BasePart physics / misc
	"Locked", "Velocity", "RotVelocity", "AssemblyLinearVelocity",
	"AssemblyAngularVelocity", "AssemblyMass", "PivotOffset", "WorldPivot",
	"RootPriority", "CustomPhysicalProperties", "CollisionGroupId",
	"LocalTransparencyModifier", "ReceiveAge", "NetworkOwnershipAuto",
	-- DataModel / services
	"HttpEnabled", "LoadStringEnabled", "StreamingEnabled",
	"StreamingTargetRadius", "Gravity", "FallenPartsDestroyHeight",
	-- Lighting
	"Ambient", "OutdoorAmbient", "GlobalShadows", "GeographicLatitude",
	"ClockTime", "TimeOfDay", "FogStart", "FogEnd", "FogColor",
	"EnvironmentSpecularScale", "EnvironmentDiffuseScale", "Technology",
	-- Model
	"PrimaryPart", "ModelStreamingMode", "LevelOfDetail", "ScaleTo",
	-- Gui containers
	"AutoLocalize", "RootLocalizationTable", "ClipsDescendants",
	"SelectionImageObject", "SelectionOrder", "NextSelectionLeft",
	"NextSelectionRight", "NextSelectionUp", "NextSelectionDown",
	"SizeConstraint",
	-- ScrollingFrame
	"CanvasSize", "CanvasPosition", "AutomaticCanvasSize",
	"ScrollingDirection", "ScrollBarThickness", "ScrollBarImageColor3",
	"ScrollBarImageTransparency", "BottomImage", "MidImage", "TopImage",
	"ElasticBehavior", "ScrollingEnabled", "VerticalScrollBarInset",
	"HorizontalScrollBarInset", "VerticalScrollBarPosition",
	-- TextBox
	"PlaceholderText", "PlaceholderColor3", "ClearTextOnFocus", "MultiLine",
	"TextEditable", "CursorPosition", "SelectionStart",
	-- Buttons
	"AutoButtonColor", "HoverColor3", "PressedColor3",
	-- Sound / ParticleEmitter extras
	"MaxDistance", "EmitterSize", "DistanceFactor", "DopplerScale",
	"RollOffGain", "AmbientReverb", "TimeScale", "PlaybackRegion",
	"PlaybackRegionsEnabled", "ZOffset", "LockedToPart",
	-- UIScale / text
	"UIScale", "MaxVisibleGraphemes",
	-- Generic holders
	"Value",
}

-- Pick the property-name list used to populate `inspectInstance`.
-- Prefers `inst:GetProperties()` when this Studio build exposes it (yields
-- the exact list valid for this class); otherwise falls back to a broader
-- pcall sweep built from COMMON_PROPS + BROAD_INSPECT_PROPS, deduped,
-- original order preserved.
local function buildInspectPropsList(inst)
	local ok, raw = pcall(function() return inst:GetProperties() end)
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
		if #names > 0 then return names end
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
		local ok, raw = pcall(function() return inst[name] end)
		if ok and raw ~= nil then
			local eok, encoded = pcall(encodeRemoteValue, raw)
			if eok then props[name] = encoded end
		end
	end
	local attrs = {}
	local aok, rawAttrs = pcall(function() return inst:GetAttributes() end)
	if aok and typeof(rawAttrs) == "table" then
		for k, v in pairs(rawAttrs) do
			local eok, encoded = pcall(encodeRemoteValue, v)
			if eok then attrs[tostring(k)] = encoded end
		end
	end
	local tags = {}
	local tok, rawTags = pcall(function() return game:GetService("CollectionService"):GetTags(inst) end)
	if tok and typeof(rawTags) == "table" then
		for _, t in ipairs(rawTags) do table.insert(tags, tostring(t)) end
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
	if not inst then error("path not found: " .. tostring(args.path)) end
	local prop = tostring(args.prop or "")
	if prop == "" then return inspectInstance(inst) end
	return encodeRemoteValue(inst[prop])
end

remoteHandlers.set = function(args)
	local inst = resolveRemotePath(tostring(args.path or ""))
	if not inst then error("path not found: " .. tostring(args.path)) end
	local prop = tostring(args.prop or "")
	if prop == "" then error("missing prop") end
	local decoded = decodeRemoteValue(args.value)
	-- Route script Source through ScriptEditorService so Studio's open editor
	-- sees the new text, matching applyProperties behavior for daemon-driven ops.
	if prop == "Source" and inst:IsA("LuaSourceContainer") then
		writeScriptSource(inst, tostring(decoded or ""))
	else
		inst[prop] = decoded
	end
	return { path = instancePathString(inst), prop = prop }
end

remoteHandlers.ls = function(args)
	local inst = resolveRemotePath(tostring(args.path or ""))
	if not inst then error("path not found: " .. tostring(args.path)) end
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
		if #inst:GetChildren() > 0 then node.truncated = true end
		return node
	end
	for _, child in ipairs(inst:GetChildren()) do
		table.insert(node.children, buildRemoteTree(child, remaining - 1))
	end
	return node
end

remoteHandlers.tree = function(args)
	local inst = resolveRemotePath(tostring(args.path or ""))
	if not inst then error("path not found: " .. tostring(args.path)) end
	local depth = tonumber(args.depth) or TREE_DEPTH_DEFAULT
	if depth < 0 then depth = 0 end
	return buildRemoteTree(inst, math.floor(depth))
end

remoteHandlers.find = function(args)
	-- Accept either `under` (new, scoped traversal) or `root` (legacy).
	-- Empty string resolves to the DataModel root.
	local rootPath = args.under or args.root or ""
	local root = resolveRemotePath(tostring(rootPath))
	if not root then error("root not found: " .. tostring(rootPath)) end
	local className = args.className and tostring(args.className) or nil
	local nameFilter = args.name and tostring(args.name) or nil
	local limit = tonumber(args.limit) or FIND_LIMIT_DEFAULT
	local out = {}
	local function walk(inst)
		for _, c in ipairs(inst:GetChildren()) do
			if #out >= limit then return end
			local classOk = className == nil
			if not classOk then
				classOk = c.ClassName == className
				if not classOk then
					local ok, isA = pcall(function() return c:IsA(className) end)
					if ok and isA then classOk = true end
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
	if src == "" then error("missing source") end
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
	if not fn then error("compile: " .. tostring(compileErr)) end
	local setEnvOk, setEnvErr = pcall(function() setfenv(fn, env) end)
	if not setEnvOk then error("sandbox: " .. tostring(setEnvErr)) end
	local results = table.pack(pcall(fn))
	local ok = results[1]
	if not ok then error("runtime: " .. tostring(results[2])) end
	local n = results.n - 1
	if n == 0 then return nil end
	if n == 1 then return encodeRemoteValue(results[2]) end
	local enc = {}
	for i = 1, n do enc[i] = encodeRemoteValue(results[i + 1]) end
	return enc
end

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
	if not parent then error("parent not found: " .. parentPath) end
	local className = tostring(args.class or "")
	if className == "" then error("missing class") end
	local createOk, inst = pcall(Instance.new, className)
	if not createOk or not inst then
		error("Instance.new failed for class " .. className .. ": " .. tostring(inst))
	end
	if args.name ~= nil then inst.Name = tostring(args.name) end
	local props = args.initial_props or args.props
	if type(props) == "table" then
		for k, v in pairs(props) do
			local key = tostring(k)
			local decoded = decodeRemoteValue(v)
			-- Skip Parent here — we set it last so every other property is in
			-- place before AncestryChanged fires.
			if key == "Source" and inst:IsA("LuaSourceContainer") then
				writeScriptSource(inst, tostring(decoded or ""))
			elseif key ~= "Parent" then
				inst[key] = decoded
			end
		end
	end
	inst.Parent = parent
	return { path = instancePathString(inst), class = inst.ClassName, name = inst.Name }
end

remoteHandlers.rm = function(args)
	local inst = resolveRemotePath(tostring(args.path or ""))
	if not inst then error("path not found: " .. tostring(args.path)) end
	if inst == game then error("cannot destroy the DataModel root") end
	local path = instancePathString(inst)
	inst:Destroy()
	return { path = path, destroyed = true }
end

remoteHandlers.mv = function(args)
	local fromPath = tostring(args.from or "")
	local toPath = tostring(args.to or "")
	local inst = resolveRemotePath(fromPath)
	if not inst then error("from not found: " .. fromPath) end
	if inst == game then error("cannot reparent the DataModel root") end
	local newParent = resolveRemotePath(toPath)
	if not newParent then error("to not found: " .. toPath) end
	-- Top-level service boundary = segs[1]. Cross-service reparenting requires
	-- args.force because dropping an instance into the wrong service
	-- (e.g. Workspace -> ServerStorage) usually means losing replication.
	local oldSegs = splitRemotePath(instancePathString(inst))
	local newParentSegs = splitRemotePath(instancePathString(newParent))
	local oldService = oldSegs[1] or ""
	local newService = newParentSegs[1] or ""
	if oldService ~= newService and not args.force then
		error(string.format(
			"refusing to reparent across service boundary %q -> %q (pass force=true to override)",
			oldService, newService))
	end
	inst.Parent = newParent
	return { path = instancePathString(inst), parent = instancePathString(newParent) }
end

remoteHandlers.set_attr = function(args)
	local inst = resolveRemotePath(tostring(args.path or ""))
	if not inst then error("path not found: " .. tostring(args.path)) end
	local name = tostring(args.name or "")
	if name == "" then error("missing attribute name") end
	local decoded = decodeRemoteValue(args.value)
	inst:SetAttribute(name, decoded)
	return { path = instancePathString(inst), name = name }
end

remoteHandlers.rm_attr = function(args)
	local inst = resolveRemotePath(tostring(args.path or ""))
	if not inst then error("path not found: " .. tostring(args.path)) end
	local name = tostring(args.name or "")
	if name == "" then error("missing attribute name") end
	inst:SetAttribute(name, nil)
	return { path = instancePathString(inst), name = name, cleared = true }
end

remoteHandlers.attr_ls = function(args)
	local inst = resolveRemotePath(tostring(args.path or ""))
	if not inst then error("path not found: " .. tostring(args.path)) end
	local out = {}
	local aok, raw = pcall(function() return inst:GetAttributes() end)
	if aok and typeof(raw) == "table" then
		for k, v in pairs(raw) do
			local eok, encoded = pcall(encodeRemoteValue, v)
			if eok then out[tostring(k)] = encoded end
		end
	end
	return out
end

remoteHandlers.add_tag = function(args)
	local inst = resolveRemotePath(tostring(args.path or ""))
	if not inst then error("path not found: " .. tostring(args.path)) end
	local tag = tostring(args.tag or "")
	if tag == "" then error("missing tag") end
	CollectionService:AddTag(inst, tag)
	return { path = instancePathString(inst), tag = tag, added = true }
end

remoteHandlers.rm_tag = function(args)
	local inst = resolveRemotePath(tostring(args.path or ""))
	if not inst then error("path not found: " .. tostring(args.path)) end
	local tag = tostring(args.tag or "")
	if tag == "" then error("missing tag") end
	CollectionService:RemoveTag(inst, tag)
	return { path = instancePathString(inst), tag = tag, removed = true }
end

remoteHandlers.call = function(args)
	local inst = resolveRemotePath(tostring(args.path or ""))
	if not inst then error("path not found: " .. tostring(args.path)) end
	local method = tostring(args.method or "")
	if method == "" then error("missing method") end
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
	if callResults.n == 0 then return nil end
	if callResults.n == 1 then return encodeRemoteValue(callResults[1]) end
	local callOut = {}
	for i = 1, callResults.n do callOut[i] = encodeRemoteValue(callResults[i]) end
	return callOut
end

remoteHandlers.select_get = function(_args)
	local out = {}
	local selOk, sel = pcall(function() return Selection:Get() end)
	if selOk and typeof(sel) == "table" then
		for _, inst in ipairs(sel) do
			table.insert(out, instancePathString(inst))
		end
	end
	return out
end

remoteHandlers.select_set = function(args)
	local paths = args.paths
	if type(paths) ~= "table" then error("paths must be an array") end
	local list = {}
	for i, p in ipairs(paths) do
		local inst = resolveRemotePath(tostring(p))
		if not inst then error("path not found (index " .. i .. "): " .. tostring(p)) end
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
		local ok, err = pcall(function() game:Save() end)
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
	if name == "" then error("missing name") end
	ChangeHistoryService:SetWaypoint(name)
	return { ok = true, name = name }
end

remoteHandlers.version = function(_args)
	local studioVersion
	local ok, v = pcall(function() return version() end)
	if ok then studioVersion = tostring(v) end
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
			"GetMass", "GetNetworkOwner", "GetNetworkOwnershipAuto",
			"GetRootPart", "GetTouchingParts", "GetConnectedParts",
			"GetJoints", "GetPivot", "IsA", "IsDescendantOf", "IsAncestorOf",
			"FindFirstChild", "FindFirstAncestor", "FindFirstAncestorOfClass",
			"FindFirstAncestorWhichIsA", "GetChildren", "GetDescendants",
			"GetFullName", "Destroy", "Clone",
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
			"GetBoundingBox", "GetExtentsSize", "GetPivot", "PivotTo",
			"MoveTo", "TranslateBy", "ScaleTo", "GetScale",
			"Destroy", "Clone", "IsA", "GetChildren", "GetDescendants", "GetFullName",
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
			"Raycast", "FindPartOnRay", "FindPartOnRayWithIgnoreList",
			"GetPartBoundsInBox", "GetPartsInPart", "GetDescendants", "GetChildren",
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
	if className == "" then error("missing class_name") end
	-- Try StudioService reflection first; fall back to the baked table.
	-- StudioService:GetClassProperties isn't exposed on every Studio build,
	-- so any failure is treated as "not available".
	local ok, ss = pcall(function() return game:GetService("StudioService") end)
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
					if type(vt) == "table" then typeStr = tostring(vt.Name or vt)
					elseif vt ~= nil then typeStr = tostring(vt)
					else typeStr = tostring(p.Type or p.type or "") end
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
	if enumName == "" then error("missing enum_name") end
	local ok, enum = pcall(function() return Enum[enumName] end)
	if not ok or enum == nil then error("unknown enum: " .. enumName) end
	local itemsOk, rawItems = pcall(function() return enum:GetEnumItems() end)
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
	local ok, enums = pcall(function() return Enum:GetEnums() end)
	if not ok or type(enums) ~= "table" then
		error("Enum:GetEnums() not available")
	end
	local out = {}
	for _, e in ipairs(enums) do
		local s = tostring(e)
		if s:sub(1, 5) == "Enum." then s = s:sub(6) end
		table.insert(out, s)
	end
	table.sort(out)
	return out
end

remoteHandlers.find_by_attr = function(args)
	local attrName = tostring(args.name or "")
	if attrName == "" then error("missing name") end
	local rootPath = args.under or args.root or ""
	local root = resolveRemotePath(tostring(rootPath))
	if not root then error("root not found: " .. tostring(rootPath)) end
	local limit = tonumber(args.limit) or FIND_LIMIT_DEFAULT
	local hasValue = args.value ~= nil
	local target = nil
	if hasValue then
		local dok, decoded = pcall(decodeRemoteValue, args.value)
		if not dok then error("decode value: " .. tostring(decoded)) end
		target = decoded
	end
	local function equal(a, b)
		if a == b then return true end
		local ta, tb = typeof(a), typeof(b)
		if ta ~= tb then return false end
		-- Userdata types whose == isn't structural — fall back to tostring.
		if ta == "Vector3" or ta == "Vector2" or ta == "Color3"
			or ta == "UDim" or ta == "UDim2" or ta == "CFrame"
			or ta == "BrickColor" or ta == "EnumItem" or ta == "NumberRange" then
			return tostring(a) == tostring(b)
		end
		return false
	end
	local out = {}
	local function walk(inst)
		for _, c in ipairs(inst:GetChildren()) do
			if #out >= limit then return end
			local ok, val = pcall(function() return c:GetAttribute(attrName) end)
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

local REMOTE_WRITE_OPS = {
	set = true,
	eval = true,
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
}

local function postRemoteWriteLog(entry)
	task.spawn(function()
		local ok, err = httpRequest("/writelog", "POST", entry)
		if not ok then
			warn("[Ro Sync] writelog POST failed: " .. tostring(err))
		end
	end)
end

local function handleRemoteRequest(client, msg)
	local reqId = msg.request_id
	if type(reqId) ~= "number" then
		-- Without a request_id we have nothing to correlate a reply to — drop.
		log("ws: request missing numeric request_id")
		return
	end
	local op = tostring(msg.op or "")
	local args = msg.args
	if type(args) ~= "table" then args = {} end
	local handler = remoteHandlers[op]
	if not handler then
		wsSend(client, { type = "response", request_id = reqId, ok = false,
			error = "unknown op: " .. op })
		return
	end
	local ok, result = pcall(handler, args)
	if ok then
		wsSend(client, { type = "response", request_id = reqId, ok = true, value = result })
		if REMOTE_WRITE_OPS[op] then
			postRemoteWriteLog({
				ts = os.time(),
				op = op,
				args = args,
				ok = true,
				result = result,
			})
		end
	else
		wsSend(client, { type = "response", request_id = reqId, ok = false,
			error = tostring(result) })
	end
end

local function wsLoop(gen)
	local clientId = tostring(game.GameId)
	while connected and gen == wsGeneration do
		local url = toWsUrl(baseUrl)
		log("ws: connecting to " .. url)
		local client
		-- CreateWebStreamClient takes a CONFIG TABLE, not a bare URL string.
		-- Roblox rejects arg 2 with "unable to cast string to dictionary" if a
		-- string is passed. The documented field is `Url`.
		local createOk, createErr = pcall(function()
			client = HttpService:CreateWebStreamClient(
				Enum.WebStreamClientType.WebSocket,
				{ Url = url }
			)
		end)
		local needResyncAfter = false
		if not createOk or not client then
			log("ws: create err: " .. tostring(createErr))
		else
			ws = client
			local closed = false
			local lagged = false
			local lastPongClock = os.clock()
			local conns: {RBXScriptConnection} = {}

			table.insert(conns, client.MessageReceived:Connect(function(payload)
				local msg = parseWsPayload(payload)
				if not msg then return end
				-- Any server traffic counts as liveness for the heartbeat
				-- watchdog — the daemon pings every 10s on its own schedule,
				-- so we don't need to see a pong specifically.
				lastPongClock = os.clock()
				local kind = tostring(msg.type or "")
				if kind == "op" then
					-- Daemon wraps the op under msg.op (nested, per Agent 1).
					if type(msg.op) == "table" then
						applyOps({ msg.op })
					else
						log("ws: op frame missing nested op payload")
					end
				elseif kind == "lagged" then
					log("ws: daemon broadcast lagged — will resync via /initial-compare")
					lagged = true
					pcall(function() client:Close() end)
				elseif kind == "pong" then
					-- liveness already refreshed above
				elseif kind == "ping" then
					wsSend(client, { type = "pong" })
				elseif kind == "push-result" then
					if msg.ok == false or (msg.errors and #msg.errors > 0) then
						log(string.format("ws push-result: ok=%s applied=%s skipped=%s errs=%s",
							tostring(msg.ok), tostring(msg.applied), tostring(msg.skipped),
							tostring(msg.errors and #msg.errors or 0)))
					end
				elseif kind == "request" then
					handleRemoteRequest(client, msg)
				elseif kind == "error" then
					log("ws error: " .. tostring(msg.error or "?"))
				elseif kind == "conflict" or kind == "config-changed"
					or kind == "initial-choice-needed" or kind == "initial-choice-made" then
					-- Events forwarded from daemon.state.events. The plugin's
					-- initial-sync path uses HTTP /initial-decision polling, so
					-- these are informational here.
					log("ws event: " .. kind)
				else
					log("ws: unknown msg type " .. tostring(kind))
				end
			end))
			table.insert(conns, client.Closed:Connect(function()
				closed = true
			end))

			-- Announce ourselves. If the initial Send fails outright, bail and
			-- let the outer backoff loop retry.
			local helloOk = wsSend(client, { type = "hello", clientId = clientId })
			if helloOk then
				wsBackoff = WS_BACKOFF_START
				lastPongClock = os.clock()
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
						pcall(function() client:Close() end)
						break
					end
				end
			else
				log("ws: hello send failed")
			end

			for _, c in ipairs(conns) do
				pcall(function() c:Disconnect() end)
			end
			pcall(function() client:Close() end)
			if ws == client then ws = nil end
			if lagged then needResyncAfter = true end
		end

		if not connected or gen ~= wsGeneration then break end

		if needResyncAfter then
			-- Tear down Studio hooks and re-enter the full initial-sync handshake.
			-- Bumping wsGeneration ensures any other in-flight wsLoop iteration
			-- exits, and setting connected=false means the subsequent
			-- runInitialCompare -> startHooksAndWs path is responsible for
			-- restoring the connected state.
			log("ws: re-running /initial-compare after lag")
			disconnectHooks()
			connected = false
			wsGeneration += 1
			task.spawn(function()
				busy = true
				setPill("connecting")
				runInitialCompare()
			end)
			return
		end

		local waitSec = wsBackoff
		wsBackoff = math.min(wsBackoff * 2, WS_BACKOFF_MAX)
		log(string.format("ws: reconnecting in %ds", waitSec))
		local waited = 0
		while waited < waitSec and connected and gen == wsGeneration do
			task.wait(0.25)
			waited += 0.25
		end
	end
end

local function startHooksAndWs()
	installHooks()
	connected = true
	busy = false
	wsGeneration += 1
	wsBackoff = WS_BACKOFF_START
	local gen = wsGeneration
	setPill("connected", tostring(connectedPort or "?"))
	setBanner(nil)
	log("connected — watching " .. #WATCHED_SERVICES .. " services")
	task.spawn(function() wsLoop(gen) end)
	-- Seed tree.json with the current Studio skeleton. Spawned so the HTTP
	-- POST doesn't block the hook-installation path. Subsequent updates are
	-- debounced through markTreeDirty from onDescendantAdded/Removing/Changed.
	treeDirty = true
	task.spawn(pushTreeSkeleton)
end

local function doPushPath()
	log("initial-compare → push: sending Studio snapshot")
	local services = collectSnapshot()

	-- Push one service per HTTP call. A monolithic bootstrap body of N
	-- services could easily exceed Studio's 30s RequestAsync timeout on a
	-- big project; if that timed out mid-write the daemon could end up with
	-- a partially-written disk, which then (on a later pull) could try to
	-- re-parent Studio services as Folders. Splitting per-service keeps
	-- every call small, and any one failure only costs that service.
	local okCount, skipCount = 0, 0
	local firstErr = nil
	for _, svc in ipairs(services) do
		local svcName = (svc and svc.name) or "?"
		local encOk, encErr = pcall(function() return HttpService:JSONEncode(svc) end)
		if not encOk then
			log(string.format("  ✗ %s — cannot encode (%s); skipping",
				svcName, tostring(encErr):sub(1, 200)))
			skipCount += 1
		else
			local ok, err = httpJson("/push", "POST", {
				bootstrap = true,
				services = { svc },
			})
			if ok then
				okCount += 1
			else
				skipCount += 1
				firstErr = firstErr or tostring(err)
				log(string.format("  ✗ %s — push err: %s",
					svcName, tostring(err):sub(1, 200)))
			end
		end
	end

	if okCount == 0 then
		setPill("error", "push failed")
		log(string.format("bootstrap failed: 0/%d services pushed (first err: %s)",
			#services, tostring(firstErr):sub(1, 200)))
		busy = false
		return false
	end

	log(string.format("bootstrap done: %d/%d services pushed (%d skipped)",
		okCount, #services, skipCount))
	return true
end

local function doPullPath()
	log("initial-compare → pull: applying disk snapshot to Studio")
	local ok, snap = httpJson("/snapshot", "GET")
	if not ok then
		log("snapshot err: " .. tostring(snap))
		setPill("error", "snapshot failed")
		busy = false
		return false
	end
	applySnapshot(snap)
	return true
end

local function disconnect(reason)
	connected = false
	busy = false
	wsGeneration += 1
	decisionGeneration += 1
	disconnectHooks()
	if ws then
		local toClose = ws
		ws = nil
		pcall(function() toClose:Close() end)
	end
	setPill("idle")
	setBanner(nil)
	connectedPort = nil
	if reason then log("disconnected: " .. reason) end
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
		if gen ~= decisionGeneration then return nil end
		if not ok then
			log("decision poll err: " .. tostring(resp))
			task.wait(2)
		elseif not resp.Success then
			if resp.StatusCode == 408 or resp.StatusCode == 204 then
				-- idle timeout, re-issue
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

-- Assigned to the forward-declared local at the top of the HTTP helpers
-- section so wsLoop's lagged-resync path can reach it without a circular decl.
runInitialCompare = function()
	local studioStats = collectStudioStats()
	log(string.format("initial-compare: scripts=%d instances=%d",
		studioStats.scriptCount, studioStats.instanceCount))
	local ok, resp = httpJson("/initial-compare", "POST", { studioStats = studioStats })
	if not ok then
		log("initial-compare err: " .. tostring(resp))
		setPill("error", "compare failed")
		busy = false
		return
	end
	if type(resp) ~= "table" or not resp.action then
		log("initial-compare: malformed response")
		setPill("error", "bad compare")
		busy = false
		return
	end
	local action = tostring(resp.action)
	if action == "in-sync" then
		-- Daemon says disk and Studio already match within tolerance — skip
		-- the overwrite prompt and start hooks directly.
		log("initial-compare: already in sync, skipping decision modal")
		startHooksAndWs()
	elseif action == "push" then
		if doPushPath() then startHooksAndWs() end
	elseif action == "pull" then
		if doPullPath() then startHooksAndWs() end
	elseif action == "decide" then
		local choiceId = resp.choiceId
		local diskStats = resp.diskStats or {}
		log(string.format("decide: studio=%s/%s disk=%s/%s",
			tostring(studioStats.scriptCount), tostring(studioStats.instanceCount),
			tostring(diskStats.scriptCount), tostring(diskStats.instanceCount)))
		decisionGeneration += 1
		local gen = decisionGeneration
		task.spawn(function()
			local choice = waitForDecision(choiceId, gen)
			if gen ~= decisionGeneration then return end
			if choice == "disk" then
				if doPullPath() then startHooksAndWs() end
			elseif choice == "studio" then
				if doPushPath() then startHooksAndWs() end
			elseif choice == "cancel" then
				log("decision: cancel")
				disconnect("cancelled by user")
			else
				log("decision: unknown/aborted")
				disconnect("decision aborted")
			end
		end)
	else
		log("initial-compare: unknown action " .. action)
		setPill("error", "bad action")
		busy = false
	end
end

--------------------------------------------------------------------------------
-- Port probing / auto-discovery
--------------------------------------------------------------------------------

local function trimTrailingSlash(s)
	return (s:gsub("/+$", ""))
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

local function discoverDaemon(gameId)
	-- Spawn a coroutine per port, first-match-wins via a shared result slot.
	setPill("probing")
	log(string.format("probing ports %d..%d for GameId %s",
		PORT_RANGE_START, PORT_RANGE_END, gameId))
	local result = { found = false, url = nil, port = nil, sawWrongGame = false }
	local remaining = PORT_RANGE_END - PORT_RANGE_START + 1
	local done = false

	for port = PORT_RANGE_START, PORT_RANGE_END do
		task.spawn(function()
			local matched, url, hello = probePort(port, gameId)
			if done then
				remaining -= 1
				return
			end
			if matched then
				result.found = true
				result.url = url
				result.port = port
				done = true
			elseif hello ~= nil then
				result.sawWrongGame = true
			end
			remaining -= 1
		end)
	end

	-- Wait up to PROBE_TIMEOUT_SECONDS, or until a match or all done.
	local start = os.clock()
	while not done and remaining > 0 and (os.clock() - start) < PROBE_TIMEOUT_SECONDS do
		task.wait(0.05)
	end
	done = true
	return result
end

--------------------------------------------------------------------------------
-- Connect / Disconnect entry point
--------------------------------------------------------------------------------

local function startConnect()
	if connected then
		disconnect("user")
		return
	end
	if busy then
		-- Cancel in-flight probing/connecting/waiting
		busy = false
		wsGeneration += 1
		decisionGeneration += 1
		setPill("idle")
		setBanner(nil)
		log("cancelled")
		return
	end

	busy = true

	-- Persist manual URL if the user typed one
	local manualUrl = urlBox.Text
	if manualUrl == nil then manualUrl = "" end
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
				pcall(function() hello = HttpService:JSONDecode(resp.Body or "") end)
				if type(hello) == "table" and hello.gameId then
					if gameId ~= "0" and tostring(hello.gameId) ~= gameId then
						log(string.format("warning: manual URL reports gameId=%s but Studio is %s",
							tostring(hello.gameId), gameId))
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
			while waiting do task.wait(0.05) end
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
			local result = discoverDaemon(gameId)
			if not result.found then
				if result.sawWrongGame then
					setPill("wrong_game")
					log("found a daemon on a probed port, but no gameId match")
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

		baseUrl = trimTrailingSlash(chosenUrl)
		connectedPort = chosenPort
		setPill("connecting")

		-- Initial-sync handshake before hooks + ws
		runInitialCompare()
	end)
end

connectBtn.Activated:Connect(startConnect)

urlBox.FocusLost:Connect(function(enter)
	if enter then
		plugin:SetSetting(SETTING_MANUAL_URL, trimTrailingSlash(urlBox.Text or ""))
	end
end)

plugin.Unloading:Connect(function()
	disconnect("unloading")
end)

setPill("idle")
log("Ro Sync loaded. Click Connect to auto-discover the daemon.")
