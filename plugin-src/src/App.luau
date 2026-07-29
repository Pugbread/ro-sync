--!nonstrict

local Packages = (script.Parent :: Instance):WaitForChild("Packages")
local React = require(Packages:WaitForChild("React"))
local ReactRoblox = require(Packages:WaitForChild("ReactRoblox"))

local MAX_LOG_LINES = 200
local DEFAULT_LOGO_IMAGE = "rbxassetid://134480165435830"
local DEFAULT_SETTINGS_IMAGE = "rbxassetid://128477924144442"

local Colors = {
	Panel = Color3.fromRGB(25, 26, 31),
	PanelRaised = Color3.fromRGB(35, 37, 40),
	Field = Color3.fromRGB(29, 31, 34),
	Stroke = Color3.fromRGB(255, 255, 255),
	Text = Color3.fromRGB(244, 245, 247),
	Muted = Color3.fromRGB(171, 175, 182),
	Subtle = Color3.fromRGB(119, 125, 134),
	Accent = Color3.fromRGB(65, 135, 245),
	AccentDark = Color3.fromRGB(43, 101, 199),
	Good = Color3.fromRGB(74, 193, 123),
	Bad = Color3.fromRGB(207, 72, 72),
	Overlay = Color3.fromRGB(0, 0, 0),
}

local function normalizeManualUrl(value)
	local trimmed = tostring(value or ""):match("^%s*(.-)%s*$") or ""
	return (trimmed:gsub("/+$", ""))
end

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
		return "Auto-discover localhost:7878–7890", Colors.Subtle, "Connect"
	elseif state == "probing" then
		return "Scanning localhost...", Colors.Accent, "Cancel"
	elseif state == "connecting" then
		return "Connecting...", Colors.Accent, "Cancel"
	elseif state == "reconnecting" then
		return string.format("Retrying in %ss...", tostring(detail or "?")), Colors.Accent, "Cancel"
	elseif state == "waiting" then
		return "Waiting for initial sync...", Colors.Accent, "Cancel"
	elseif state == "project_init" then
		return "New Studio project · ready to create", Colors.Accent, "Create Project"
	elseif state == "project_creating" then
		return "Creating Ro Sync project...", Colors.Accent, "Cancel"
	elseif state == "project_waiting" then
		return "Project ready · waiting for Desktop...", Colors.Accent, "Cancel"
	elseif state == "daemon_waiting" then
		return string.format("Waiting for Desktop... %ss", tostring(detail or "?")), Colors.Accent, "Cancel"
	elseif state == "connected" then
		return string.format("localhost:%s", tostring(detail or "?")), Colors.Good, "Disconnect"
	elseif state == "no_daemon" then
		return string.format("No daemon for GameId %s", tostring(detail or "?")), Colors.Bad, "Connect"
	elseif state == "wrong_game" then
		return "Daemon belongs to another game", Colors.Bad, "Connect"
	elseif state == "error" then
		return tostring(detail or "Connection error"), Colors.Bad, "Connect"
	end
	return tostring(state or "Ready"), Colors.Subtle, "Connect"
end

local function button(props)
	return React.createElement("TextButton", {
		Size = props.Size or UDim2.new(0, 122, 0, 40),
		Position = props.Position,
		AnchorPoint = props.AnchorPoint,
		LayoutOrder = props.LayoutOrder,
		BackgroundColor3 = props.Color or Colors.Accent,
		BackgroundTransparency = props.BackgroundTransparency or 0,
		AutoButtonColor = true,
		BorderSizePixel = 0,
		Text = props.Text or "",
		TextColor3 = props.TextColor3 or Colors.Text,
		Font = Enum.Font.SourceSansBold,
		TextSize = 15,
		ZIndex = props.ZIndex or 1,
		[React.Event.Activated] = props.OnActivated,
	}, {
		Corner = corner(6),
		Stroke = props.StrokeColor and stroke(props.StrokeColor, 1, props.StrokeTransparency or 0.88) or nil,
		Icon = props.Image and React.createElement("ImageLabel", {
			AnchorPoint = Vector2.new(0.5, 0.5),
			Position = UDim2.fromScale(0.5, 0.5),
			Size = props.ImageSize or UDim2.fromOffset(32, 32),
			BackgroundTransparency = 1,
			Image = props.Image,
			ImageColor3 = props.ImageColor3 or Colors.Text,
			ScaleType = Enum.ScaleType.Fit,
		}) or nil,
	})
end

local function textInput(props)
	return React.createElement("TextBox", {
		LayoutOrder = props.LayoutOrder,
		Size = props.Size or UDim2.new(1, 0, 0, 40),
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
		Corner = corner(6),
		Stroke = stroke(Colors.Stroke, 1, 0.9),
		Pad = padding(0, 12, 0, 12),
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

local function logList(logs, layoutOrder, size)
	local children = {
		Layout = React.createElement("UIListLayout", {
			SortOrder = Enum.SortOrder.LayoutOrder,
			Padding = UDim.new(0, 3),
		}),
		Pad = padding(8, 10, 8, 10),
		Corner = corner(6),
		Stroke = stroke(Colors.Stroke, 1, 0.92),
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
		LayoutOrder = layoutOrder,
		Size = size or UDim2.new(1, 0, 0, 116),
		BackgroundColor3 = Colors.Field,
		BorderSizePixel = 0,
		AutomaticCanvasSize = Enum.AutomaticSize.Y,
		CanvasSize = UDim2.fromScale(0, 0),
		ScrollBarThickness = 6,
		ScrollBarImageColor3 = Colors.Subtle,
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
			Size = UDim2.new(1, -36, 0, 222),
			BackgroundColor3 = Colors.PanelRaised,
			BorderSizePixel = 0,
			ZIndex = 51,
		}, {
			Corner = corner(8),
			Stroke = stroke(Colors.Stroke, 1, 0.88),
			Pad = padding(16, 16, 16, 16),
			Layout = React.createElement("UIListLayout", {
				SortOrder = Enum.SortOrder.LayoutOrder,
				Padding = UDim.new(0, 10),
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
				Size = UDim2.new(1, 0, 0, 40),
				BackgroundTransparency = 1,
				ZIndex = 52,
			}, {
				Cancel = button({
					Size = UDim2.new(0, 90, 1, 0),
					Position = UDim2.new(1, -204, 0, 0),
					Color = Colors.PanelRaised,
					StrokeColor = Colors.Stroke,
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
	local currentPlaceName = tostring(props.placeName or game.Name or "Studio place")
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

	controller.setProjectName = function(text)
		currentPlaceName = tostring(text or currentPlaceName)
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
		local settingsOpen, setSettingsOpen = React.useState(false)
		local placeName, setPlaceNameState = React.useState(currentPlaceName)

		local function setManualUrl(text)
			currentManualUrl = tostring(text or "")
			setManualUrlState(currentManualUrl)
		end

		controller.getManualUrl = function()
			return currentManualUrl
		end

		controller.setManualUrl = setManualUrl

		controller.setProjectName = function(text)
			currentPlaceName = tostring(text or currentPlaceName)
			setPlaceNameState(currentPlaceName)
		end

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
			Content = React.createElement("ScrollingFrame", {
				Size = UDim2.fromScale(1, 1),
				BackgroundTransparency = 1,
				BorderSizePixel = 0,
				AutomaticCanvasSize = Enum.AutomaticSize.Y,
				CanvasSize = UDim2.fromScale(0, 0),
				ScrollBarThickness = 4,
				ScrollBarImageColor3 = Colors.Subtle,
				ScrollingDirection = Enum.ScrollingDirection.Y,
			}, {
				Pad = padding(18, 16, 18, 16),
				Layout = React.createElement("UIListLayout", {
					SortOrder = Enum.SortOrder.LayoutOrder,
					Padding = UDim.new(0, 12),
				}),
				Header = React.createElement("Frame", {
					LayoutOrder = 1,
					Size = UDim2.new(1, 0, 0, 20),
					BackgroundTransparency = 1,
				}, {
					Logo = React.createElement("ImageLabel", {
						Position = UDim2.fromOffset(0, 1),
						Size = UDim2.fromOffset(113, 18),
						BackgroundTransparency = 1,
						Image = props.logoImage or DEFAULT_LOGO_IMAGE,
						ImageColor3 = Colors.Accent,
						ScaleType = Enum.ScaleType.Fit,
					}),
				}),
				Connection = React.createElement("Frame", {
					LayoutOrder = 2,
					Size = UDim2.new(1, 0, 0, 70),
					BackgroundColor3 = Colors.PanelRaised,
					BorderSizePixel = 0,
				}, {
					Corner = corner(7),
					Stroke = stroke(Colors.Stroke, 1, 0.88),
					Place = label({
						Position = UDim2.fromOffset(14, 9),
						Size = UDim2.new(1, -46, 0, 23),
						Font = Enum.Font.SourceSansBold,
						TextSize = 17,
						Text = placeName,
						TextTruncate = Enum.TextTruncate.AtEnd,
					}),
					Endpoint = label({
						Position = UDim2.fromOffset(14, 35),
						Size = UDim2.new(1, -28, 0, 19),
						Font = Enum.Font.Code,
						TextSize = 13,
						Text = statusText,
						Color = status.state == "connected" and Colors.Muted or statusColor,
						TextTruncate = Enum.TextTruncate.AtEnd,
					}),
					State = React.createElement("Frame", {
						AnchorPoint = Vector2.new(1, 0),
						Position = UDim2.new(1, -14, 0, 16),
						Size = UDim2.fromOffset(8, 8),
						BackgroundColor3 = statusColor,
						BorderSizePixel = 0,
					}, {
						Corner = React.createElement("UICorner", { CornerRadius = UDim.new(1, 0) }),
					}),
				}),
				Controls = React.createElement("Frame", {
					LayoutOrder = 3,
					Size = UDim2.new(1, 0, 0, 40),
					BackgroundTransparency = 1,
				}, {
					Layout = React.createElement("UIListLayout", {
						FillDirection = Enum.FillDirection.Horizontal,
						HorizontalAlignment = Enum.HorizontalAlignment.Right,
						SortOrder = Enum.SortOrder.LayoutOrder,
						Padding = UDim.new(0, 10),
					}),
					Settings = button({
						LayoutOrder = 1,
						Size = UDim2.fromOffset(40, 40),
						Color = Colors.PanelRaised,
						Text = "",
						Image = DEFAULT_SETTINGS_IMAGE,
						OnActivated = function()
							setSettingsOpen(not settingsOpen)
						end,
					}),
					Connect = button({
						LayoutOrder = 2,
						Size = UDim2.new(0, 122, 1, 0),
						Text = buttonText,
						Color = Colors.Accent,
						OnActivated = props.onConnect,
					}),
				}),
				Banner = banner ~= "" and React.createElement("TextLabel", {
					LayoutOrder = 4,
					Size = UDim2.new(1, 0, 0, 42),
					BackgroundColor3 = Colors.AccentDark,
					BorderSizePixel = 0,
					Font = Enum.Font.SourceSansBold,
					Text = banner,
					TextColor3 = Colors.Text,
					TextSize = 13,
					TextWrapped = true,
				}, {
					Corner = corner(6),
					Pad = padding(0, 12, 0, 12),
				}) or nil,
				SettingsPanel = settingsOpen and React.createElement("Frame", {
					LayoutOrder = 5,
					Size = UDim2.new(1, 0, 0, 216),
					BackgroundTransparency = 1,
				}, {
					Layout = React.createElement("UIListLayout", {
						SortOrder = Enum.SortOrder.LayoutOrder,
						Padding = UDim.new(0, 8),
					}),
					UrlLabel = label({
						LayoutOrder = 1,
						Size = UDim2.new(1, 0, 0, 16),
						Text = "Manual URL override",
						Color = Colors.Muted,
						TextSize = 13,
					}),
					Url = textInput({
						LayoutOrder = 2,
						Text = manualUrl,
						PlaceholderText = props.defaultUrl,
						OnTextChanged = setManualUrl,
						OnFocusLost = function()
							local normalized = normalizeManualUrl(currentManualUrl)
							setManualUrl(normalized)
							if props.onUrlCommit then
								props.onUrlCommit(normalized)
							end
						end,
					}),
					LogLabel = label({
						LayoutOrder = 3,
						Size = UDim2.new(1, 0, 0, 18),
						Text = "Activity",
						Color = Colors.Muted,
						TextSize = 13,
					}),
					Logs = logList(logs, 4, UDim2.new(1, 0, 0, 118)),
				}) or nil,
			}),
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
