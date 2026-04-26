--!nonstrict

local Packages = (script.Parent :: Instance):WaitForChild("Packages")
local React = require(Packages:WaitForChild("React"))
local ReactRoblox = require(Packages:WaitForChild("ReactRoblox"))

local MAX_LOG_LINES = 200

local Colors = {
	Panel = Color3.fromRGB(31, 33, 36),
	PanelRaised = Color3.fromRGB(42, 45, 49),
	Field = Color3.fromRGB(22, 24, 27),
	Stroke = Color3.fromRGB(67, 72, 78),
	Text = Color3.fromRGB(238, 240, 242),
	Muted = Color3.fromRGB(164, 170, 176),
	Subtle = Color3.fromRGB(112, 120, 128),
	Accent = Color3.fromRGB(65, 135, 245),
	AccentDark = Color3.fromRGB(43, 101, 199),
	Good = Color3.fromRGB(54, 158, 98),
	Warn = Color3.fromRGB(213, 156, 58),
	Bad = Color3.fromRGB(207, 72, 72),
	Overlay = Color3.fromRGB(0, 0, 0),
}

local function corner(radius)
	return React.createElement("UICorner", {
		CornerRadius = UDim.new(0, radius),
	})
end

local function stroke(color, thickness, transparency)
	return React.createElement("UIStroke", {
		Color = color,
		Thickness = thickness or 1,
		Transparency = transparency or 0,
	})
end

local function padding(top, right, bottom, left)
	return React.createElement("UIPadding", {
		PaddingTop = UDim.new(0, top or 0),
		PaddingRight = UDim.new(0, right or top or 0),
		PaddingBottom = UDim.new(0, bottom or top or 0),
		PaddingLeft = UDim.new(0, left or right or top or 0),
	})
end

local function statusView(state, detail)
	if state == "idle" then
		return "Idle", Colors.Subtle, "Connect"
	elseif state == "probing" then
		return "Probing...", Colors.Warn, "Cancel"
	elseif state == "connecting" then
		return "Connecting...", Colors.Warn, "Cancel"
	elseif state == "waiting" then
		return "Waiting for decision...", Colors.Warn, "Cancel"
	elseif state == "connected" then
		return string.format("Connected :%s", tostring(detail or "?")), Colors.Good, "Disconnect"
	elseif state == "no_daemon" then
		return string.format("No daemon for GameId %s", tostring(detail or "?")), Colors.Bad, "Connect"
	elseif state == "wrong_game" then
		return "Wrong game", Colors.Bad, "Connect"
	elseif state == "error" then
		return string.format("Error: %s", tostring(detail or "?")), Colors.Bad, "Connect"
	end
	return tostring(state or "Idle"), Colors.Subtle, "Connect"
end

local function button(props)
	return React.createElement("TextButton", {
		Size = props.Size or UDim2.new(0, 112, 0, 32),
		Position = props.Position,
		AnchorPoint = props.AnchorPoint,
		LayoutOrder = props.LayoutOrder,
		BackgroundColor3 = props.Color or Colors.Accent,
		AutoButtonColor = true,
		BorderSizePixel = 0,
		Text = props.Text or "",
		TextColor3 = props.TextColor3 or Colors.Text,
		Font = Enum.Font.SourceSansBold,
		TextSize = 14,
		ZIndex = props.ZIndex or 1,
		[React.Event.Activated] = props.OnActivated,
	}, {
		Corner = corner(5),
	})
end

local function textInput(props)
	return React.createElement("TextBox", {
		LayoutOrder = props.LayoutOrder,
		Size = props.Size or UDim2.new(1, 0, 0, 32),
		BackgroundColor3 = Colors.Field,
		BorderSizePixel = 0,
		ClearTextOnFocus = false,
		Font = Enum.Font.Code,
		PlaceholderText = props.PlaceholderText or "",
		PlaceholderColor3 = Colors.Subtle,
		Text = props.Text or "",
		TextColor3 = Colors.Text,
		TextSize = 13,
		TextTruncate = Enum.TextTruncate.AtEnd,
		TextXAlignment = Enum.TextXAlignment.Left,
		ZIndex = props.ZIndex or 1,
		[React.Change.Text] = function(rbx)
			if props.OnTextChanged then
				props.OnTextChanged(rbx.Text)
			end
		end,
		[React.Event.FocusLost] = function(_, enterPressed)
			if props.OnFocusLost then
				props.OnFocusLost(enterPressed)
			end
		end,
	}, {
		Corner = corner(5),
		Stroke = stroke(Colors.Stroke, 1, 0.15),
		Pad = padding(0, 8, 0, 8),
	})
end

local function label(props)
	return React.createElement("TextLabel", {
		LayoutOrder = props.LayoutOrder,
		Position = props.Position,
		AnchorPoint = props.AnchorPoint,
		Size = props.Size,
		BackgroundTransparency = 1,
		Font = props.Font or Enum.Font.SourceSans,
		Text = props.Text or "",
		TextColor3 = props.Color or Colors.Text,
		TextSize = props.TextSize or 14,
		TextWrapped = props.TextWrapped or false,
		TextTruncate = props.TextTruncate,
		TextXAlignment = props.TextXAlignment or Enum.TextXAlignment.Left,
		TextYAlignment = props.TextYAlignment or Enum.TextYAlignment.Center,
		ZIndex = props.ZIndex or 1,
	})
end

local function logList(logs)
	local children = {
		Layout = React.createElement("UIListLayout", {
			SortOrder = Enum.SortOrder.LayoutOrder,
			Padding = UDim.new(0, 3),
		}),
		Pad = padding(8, 10, 8, 10),
	}

	for index, item in ipairs(logs) do
		children["Log" .. tostring(item.id)] = label({
			LayoutOrder = index,
			Size = UDim2.new(1, -20, 0, 16),
			Font = Enum.Font.Code,
			TextSize = 12,
			Text = item.text,
			Color = Colors.Muted,
			TextTruncate = Enum.TextTruncate.AtEnd,
		})
	end

	return React.createElement("ScrollingFrame", {
		LayoutOrder = 7,
		Size = UDim2.new(1, 0, 1, -184),
		BackgroundColor3 = Colors.Field,
		BorderSizePixel = 0,
		AutomaticCanvasSize = Enum.AutomaticSize.Y,
		CanvasSize = UDim2.fromScale(0, 0),
		ScrollBarThickness = 6,
		ScrollingDirection = Enum.ScrollingDirection.Y,
	}, children)
end

local function modal(props)
	if not props.Visible then
		return nil
	end

	return React.createElement("Frame", {
		Size = UDim2.fromScale(1, 1),
		BackgroundColor3 = Colors.Overlay,
		BackgroundTransparency = 0.35,
		BorderSizePixel = 0,
		ZIndex = 50,
	}, {
		Card = React.createElement("Frame", {
			AnchorPoint = Vector2.new(0.5, 0.5),
			Position = UDim2.fromScale(0.5, 0.5),
			Size = UDim2.new(1, -36, 0, 210),
			BackgroundColor3 = Colors.PanelRaised,
			BorderSizePixel = 0,
			ZIndex = 51,
		}, {
			Corner = corner(7),
			Stroke = stroke(Colors.Stroke, 1, 0.05),
			Pad = padding(14, 14, 14, 14),
			Layout = React.createElement("UIListLayout", {
				SortOrder = Enum.SortOrder.LayoutOrder,
				Padding = UDim.new(0, 9),
			}),
			Title = label({
				LayoutOrder = 1,
				Size = UDim2.new(1, 0, 0, 22),
				Font = Enum.Font.SourceSansBold,
				TextSize = 17,
				Text = "Unpublished place",
				ZIndex = 52,
			}),
			Body = label({
				LayoutOrder = 2,
				Size = UDim2.new(1, 0, 0, 54),
				Text = "GameId is 0. Publish the place first, or enter a manual daemon URL below.",
				Color = Colors.Muted,
				TextWrapped = true,
				TextYAlignment = Enum.TextYAlignment.Top,
				ZIndex = 52,
			}),
			Url = textInput({
				LayoutOrder = 3,
				Text = props.ManualUrl,
				PlaceholderText = props.DefaultUrl,
				OnTextChanged = props.OnTextChanged,
				ZIndex = 52,
			}),
			Buttons = React.createElement("Frame", {
				LayoutOrder = 4,
				Size = UDim2.new(1, 0, 0, 32),
				BackgroundTransparency = 1,
				ZIndex = 52,
			}, {
				Cancel = button({
					Size = UDim2.new(0, 90, 1, 0),
					Position = UDim2.new(1, -202, 0, 0),
					Color = Color3.fromRGB(77, 82, 88),
					Text = "Cancel",
					OnActivated = props.OnCancel,
					ZIndex = 53,
				}),
				Use = button({
					Size = UDim2.new(0, 104, 1, 0),
					Position = UDim2.new(1, -104, 0, 0),
					Text = "Use URL",
					OnActivated = props.OnUse,
					ZIndex = 53,
				}),
			}),
		}),
	})
end

local App = {}

function App.mount(target, props)
	local controller = {}
	local root = ReactRoblox.createRoot(target)
	local currentManualUrl = tostring(props.initialUrl or "")
	local modalResolve = nil
	local nextLogId = 0
	local pendingStatus = { state = "idle", detail = nil }
	local pendingBanner = ""
	local pendingLogs = {}
	local pendingModalVisible = false

	local function appendLog(previous, item)
		local nextLogs = {}
		local startIndex = math.max(1, #previous - MAX_LOG_LINES + 2)
		for index = startIndex, #previous do
			table.insert(nextLogs, previous[index])
		end
		table.insert(nextLogs, item)
		return nextLogs
	end

	controller.getManualUrl = function()
		return currentManualUrl
	end

	controller.setManualUrl = function(text)
		currentManualUrl = tostring(text or "")
	end

	controller.setStatus = function(state, detail)
		pendingStatus = { state = state or "idle", detail = detail }
	end

	controller.setBanner = function(text)
		pendingBanner = tostring(text or "")
	end

	controller.addLog = function(text)
		nextLogId += 1
		local stamp = string.format("[%.1fs] %s", os.clock(), tostring(text))
		pendingLogs = appendLog(pendingLogs, { id = nextLogId, text = stamp })
	end

	controller.showUnpublishedModal = function(onResult)
		modalResolve = onResult
		currentManualUrl = tostring(props.getStoredManualUrl and props.getStoredManualUrl() or currentManualUrl or "")
		pendingModalVisible = true
	end

	controller.hideUnpublishedModal = function(result)
		pendingModalVisible = false
		local callback = modalResolve
		modalResolve = nil
		if callback then
			callback(result)
		end
	end

	local function RoSyncApp()
		local manualUrl, setManualUrlState = React.useState(currentManualUrl)
		local status, setStatusState = React.useState(pendingStatus)
		local banner, setBannerState = React.useState(pendingBanner)
		local logs, setLogs = React.useState(pendingLogs)
		local modalVisible, setModalVisible = React.useState(pendingModalVisible)

		local function setManualUrl(text)
			currentManualUrl = tostring(text or "")
			setManualUrlState(currentManualUrl)
		end

		controller.getManualUrl = function()
			return currentManualUrl
		end

		controller.setManualUrl = setManualUrl

		controller.setStatus = function(state, detail)
			pendingStatus = { state = state or "idle", detail = detail }
			setStatusState(pendingStatus)
		end

		controller.setBanner = function(text)
			pendingBanner = tostring(text or "")
			setBannerState(pendingBanner)
		end

		controller.addLog = function(text)
			nextLogId += 1
			local stamp = string.format("[%.1fs] %s", os.clock(), tostring(text))
			setLogs(function(previous)
				local nextLogs = appendLog(previous, { id = nextLogId, text = stamp })
				pendingLogs = nextLogs
				return nextLogs
			end)
		end

		controller.showUnpublishedModal = function(onResult)
			modalResolve = onResult
			setManualUrl(tostring(props.getStoredManualUrl and props.getStoredManualUrl() or currentManualUrl or ""))
			pendingModalVisible = true
			setModalVisible(true)
		end

		controller.hideUnpublishedModal = function(result)
			pendingModalVisible = false
			setModalVisible(false)
			local callback = modalResolve
			modalResolve = nil
			if callback then
				callback(result)
			end
		end

		local statusText, statusColor, buttonText = statusView(status.state, status.detail)

		return React.createElement("Frame", {
			Size = UDim2.fromScale(1, 1),
			BackgroundColor3 = Colors.Panel,
			BorderSizePixel = 0,
		}, {
			Pad = padding(12, 12, 12, 12),
			Layout = React.createElement("UIListLayout", {
				SortOrder = Enum.SortOrder.LayoutOrder,
				Padding = UDim.new(0, 8),
			}),
			Header = React.createElement("Frame", {
				LayoutOrder = 1,
				Size = UDim2.new(1, 0, 0, 44),
				BackgroundTransparency = 1,
			}, {
				Title = label({
					Size = UDim2.new(1, -110, 0, 24),
					Font = Enum.Font.SourceSansBold,
					TextSize = 20,
					Text = "Ro Sync",
				}),
				Sub = label({
					Position = UDim2.new(0, 0, 0, 24),
					Size = UDim2.new(1, -110, 0, 18),
					Text = "Studio daemon bridge",
					Color = Colors.Muted,
					TextSize = 13,
				}),
				StateDot = React.createElement("Frame", {
					AnchorPoint = Vector2.new(1, 0),
					Position = UDim2.new(1, 0, 0, 6),
					Size = UDim2.fromOffset(12, 12),
					BackgroundColor3 = statusColor,
					BorderSizePixel = 0,
				}, {
					Corner = React.createElement("UICorner", { CornerRadius = UDim.new(1, 0) }),
				}),
			}),
			UrlLabel = label({
				LayoutOrder = 2,
				Size = UDim2.new(1, 0, 0, 17),
				Text = "Manual URL override",
				Color = Colors.Muted,
				TextSize = 13,
			}),
			Url = textInput({
				LayoutOrder = 3,
				Text = manualUrl,
				PlaceholderText = props.defaultUrl,
				OnTextChanged = setManualUrl,
				OnFocusLost = function(enterPressed)
					if enterPressed and props.onUrlCommit then
						props.onUrlCommit(currentManualUrl)
					end
				end,
			}),
			Controls = React.createElement("Frame", {
				LayoutOrder = 4,
				Size = UDim2.new(1, 0, 0, 34),
				BackgroundTransparency = 1,
			}, {
				Connect = button({
					Size = UDim2.new(0, 112, 1, 0),
					Text = buttonText,
					Color = status.state == "connected" and Color3.fromRGB(77, 82, 88) or Colors.Accent,
					OnActivated = props.onConnect,
				}),
				Pill = React.createElement("TextLabel", {
					Position = UDim2.new(0, 122, 0, 2),
					Size = UDim2.new(1, -122, 1, -4),
					BackgroundColor3 = statusColor,
					BorderSizePixel = 0,
					Font = Enum.Font.SourceSansBold,
					Text = statusText,
					TextColor3 = Colors.Text,
					TextSize = 13,
					TextTruncate = Enum.TextTruncate.AtEnd,
				}, {
					Corner = React.createElement("UICorner", { CornerRadius = UDim.new(1, 0) }),
					Pad = padding(0, 12, 0, 12),
				}),
			}),
			Banner = banner ~= "" and React.createElement("TextLabel", {
				LayoutOrder = 5,
				Size = UDim2.new(1, 0, 0, 34),
				BackgroundColor3 = Colors.Warn,
				BorderSizePixel = 0,
				Font = Enum.Font.SourceSansBold,
				Text = banner,
				TextColor3 = Color3.fromRGB(38, 31, 19),
				TextSize = 13,
				TextWrapped = true,
			}, {
				Corner = corner(5),
				Pad = padding(0, 10, 0, 10),
			}) or nil,
			LogLabel = label({
				LayoutOrder = 6,
				Size = UDim2.new(1, 0, 0, 18),
				Text = "Log",
				Color = Colors.Muted,
				TextSize = 13,
			}),
			Logs = logList(logs),
			Modal = modal({
				Visible = modalVisible,
				ManualUrl = manualUrl,
				DefaultUrl = props.defaultUrl,
				OnTextChanged = setManualUrl,
				OnCancel = function()
					controller.hideUnpublishedModal(nil)
				end,
				OnUse = function()
					local text = (currentManualUrl or ""):gsub("^%s+", ""):gsub("%s+$", "")
					if text ~= "" then
						controller.hideUnpublishedModal(text)
					end
				end,
			}),
		})
	end

	root:render(React.createElement(RoSyncApp))

	controller.destroy = function()
		root:unmount()
	end

	return controller
end

return App
