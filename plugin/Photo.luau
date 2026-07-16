--!strict

-- Native viewport and instance rendering for Ro Sync.
--
-- The public functions return straight-alpha RGBA8 buffers. Screenshot-backed
-- EditableImages are read and destroyed inside this module, and every temporary
-- scene or UI object is removed before control returns to the caller.

local AssetService = game:GetService("AssetService")
local CaptureService = game:GetService("CaptureService")
local CoreGui = game:GetService("CoreGui")
local Players = game:GetService("Players")
local RunService = game:GetService("RunService")
local StarterGui = game:GetService("StarterGui")
local Workspace = game:GetService("Workspace")

local MAX_AXIS = 4096
local DEFAULT_TIMEOUT = 30
local DEFAULT_DELAY = 0.08
local DEFAULT_SIZE = Vector2.new(1024, 1024)
local ALPHA_NOISE_FLOOR = 0.04
local UI_ALPHA_NOISE_FLOOR = 1 / 510

export type Image = {
	size: Vector2,
	buffer: buffer,
	region: Rect?,
	fullSize: Vector2?,
	regionSource: string?,
}

export type UiMode = "none" | "overlay" | "only"

export type CaptureViewOptions = {
	region: Rect?,
	size: Vector2?,
	background: "transparent" | "scene"?,
	delay: number?,
	timeout: number?,
	alphaBleed: boolean?,
	uiMode: UiMode?,
	uiTarget: Instance?,
	-- Legacy module option. Prefer uiMode for new callers.
	hideUI: boolean?,
}

export type RenderInstanceOptions = {
	view: string?,
	direction: Vector3?,
	cameraCFrame: CFrame?,
	padding: number?,
	size: Vector2?,
	fieldOfView: number?,
	background: "transparent" | "scene"?,
	delay: number?,
	timeout: number?,
	alphaBleed: boolean?,
	uiMode: UiMode?,
	-- Legacy module option. Prefer uiMode for new callers.
	hideUI: boolean?,
	isolate: boolean?,
}

type CleanupRegistrar = (() -> ()) -> ()
type RawImage = {
	size: Vector2,
	pixels: buffer,
}

local NativePhoto = {}

local function traceError(message: any): string
	return debug.traceback(tostring(message), 2)
end

local function withCleanup<T>(body: (CleanupRegistrar) -> T): T
	local cleanupStack: { () -> () } = {}
	local function register(callback: () -> ())
		table.insert(cleanupStack, callback)
	end

	local ok, result = xpcall(function()
		return body(register)
	end, traceError)

	for index = #cleanupStack, 1, -1 do
		local cleanupOk = pcall(function()
			cleanupStack[index]()
		end)
		if not cleanupOk then
			warn("NativePhoto cleanup failed")
		end
	end

	if not ok then
		error(result, 0)
	end
	return result :: T
end

local function finite(value: number): boolean
	return value == value and value > -math.huge and value < math.huge
end

local function checkedNumber(value: any, fallback: number, minimum: number, maximum: number, label: string): number
	if value == nil then
		return fallback
	end
	if type(value) ~= "number" or not finite(value) or value < minimum or value > maximum then
		error(string.format("%s must be between %g and %g", label, minimum, maximum), 3)
	end
	return value
end

local function checkedSize(value: any, fallback: Vector2?): Vector2?
	if value == nil then
		return fallback
	end
	if typeof(value) ~= "Vector2" then
		error("size must be a Vector2", 3)
	end

	local width = value.X
	local height = value.Y
	if
		not finite(width)
		or not finite(height)
		or width < 1
		or height < 1
		or width > MAX_AXIS
		or height > MAX_AXIS
		or width % 1 ~= 0
		or height % 1 ~= 0
	then
		error(string.format("size axes must be whole numbers from 1 through %d", MAX_AXIS), 3)
	end
	return Vector2.new(width, height)
end

local function checkedBackground(value: any): "transparent" | "scene"
	if value == nil then
		return "scene"
	end
	if value ~= "transparent" and value ~= "scene" then
		error('background must be "transparent" or "scene"', 3)
	end
	return value
end

local function checkedUiMode(value: any, hideUI: boolean?): UiMode
	if value == nil then
		return if hideUI == false then "overlay" else "none"
	end
	if value ~= "none" and value ~= "overlay" and value ~= "only" then
		error('uiMode must be "none", "overlay", or "only"', 3)
	end
	return value
end

local function checkedCameraCFrame(value: any): CFrame?
	if value == nil then
		return nil
	end
	if typeof(value) ~= "CFrame" then
		error("cameraCFrame must be a CFrame", 3)
	end
	for _, component in { value:GetComponents() } do
		if not finite(component) then
			error("cameraCFrame components must be finite", 3)
		end
	end
	return value
end

local function deadlineFrom(options: { [string]: any }): (number, number)
	local timeout = checkedNumber(options.timeout, DEFAULT_TIMEOUT, 0.25, 300, "timeout")
	local delay = checkedNumber(options.delay, DEFAULT_DELAY, 0, 30, "delay")
	return os.clock() + timeout, delay
end

local function assertTime(deadline: number, label: string)
	if os.clock() >= deadline then
		error(label .. " timed out", 3)
	end
end

local function settle(delaySeconds: number, deadline: number)
	if delaySeconds <= 0 then
		RunService.Heartbeat:Wait()
		assertTime(deadline, "capture")
		return
	end
	if os.clock() + delaySeconds >= deadline then
		error("capture timed out before its requested delay elapsed", 3)
	end
	task.wait(delaySeconds)
	assertTime(deadline, "capture")
end

local function takeScreenshot(deadline: number, delaySeconds: number): RawImage
	settle(delaySeconds, deadline)

	local content: string? = nil
	local callbackFailure: string? = nil
	local requestOk, requestError = pcall(function()
		CaptureService:CaptureScreenshot(function(result: string)
			if type(result) == "string" and result ~= "" then
				content = result
			else
				callbackFailure = "capture returned no image content"
			end
		end)
	end)
	if not requestOk then
		error("unable to request screenshot: " .. tostring(requestError), 2)
	end

	while content == nil and callbackFailure == nil do
		assertTime(deadline, "screenshot request")
		RunService.Heartbeat:Wait()
	end
	if callbackFailure ~= nil then
		error(callbackFailure, 2)
	end

	assertTime(deadline, "screenshot decode")
	local editable: EditableImage? = nil
	local decodeOk, decodeError = pcall(function()
		editable = AssetService:CreateEditableImageAsync(content :: any)
	end)
	if not decodeOk or editable == nil then
		error("unable to decode screenshot: " .. tostring(decodeError), 2)
	end

	local image = editable :: EditableImage
	local readOk, readResult = pcall(function()
		local imageSize = image.Size
		if imageSize.X < 1 or imageSize.Y < 1 then
			error("decoded screenshot has invalid dimensions")
		end
		return {
			size = imageSize,
			pixels = image:ReadPixelsBuffer(Vector2.zero, imageSize),
		}
	end)
	image:Destroy()

	if not readOk then
		error("unable to read screenshot pixels: " .. tostring(readResult), 2)
	end
	assertTime(deadline, "screenshot readback")
	return readResult :: RawImage
end

local function hideScreenInterfaces(register: CleanupRegistrar)
	local states: { { item: ScreenGui, enabled: boolean } } = {}
	local seen: { [Instance]: boolean } = {}
	local roots: { Instance } = { CoreGui, StarterGui }
	local player: Player? = (Players :: any).LocalPlayer
	if player ~= nil then
		local playerGui = player:FindFirstChildOfClass("PlayerGui")
		if playerGui ~= nil then
			table.insert(roots, playerGui)
		end
	end

	for _, root in roots do
		for _, item in root:GetDescendants() do
			if item:IsA("ScreenGui") and not seen[item] then
				seen[item] = true
				local readOk, enabled = pcall(function()
					return item.Enabled
				end)
				if readOk and enabled then
					local writeOk = pcall(function()
						item.Enabled = false
					end)
					if writeOk then
						table.insert(states, { item = item, enabled = true })
					end
				end
			end
		end
	end

	register(function()
		for index = #states, 1, -1 do
			local state = states[index]
			if state.item.Parent ~= nil then
				pcall(function()
					state.item.Enabled = state.enabled
				end)
			end
		end
	end)
end

local function resample(source: RawImage, sourceRegion: Rect, outputSize: Vector2, destinationRegion: Rect?): buffer
	local sourceWidth = source.size.X
	local sourceHeight = source.size.Y
	local minX = sourceRegion.Min.X
	local minY = sourceRegion.Min.Y
	local maxX = sourceRegion.Max.X
	local maxY = sourceRegion.Max.Y

	if
		not finite(minX)
		or not finite(minY)
		or not finite(maxX)
		or not finite(maxY)
		or minX < 0
		or minY < 0
		or maxX > sourceWidth
		or maxY > sourceHeight
		or maxX <= minX
		or maxY <= minY
	then
		error("region must be a non-empty rectangle inside the screenshot", 2)
	end

	local outputWidth = outputSize.X
	local outputHeight = outputSize.Y
	local output = buffer.create(outputWidth * outputHeight * 4)
	local regionWidth = maxX - minX
	local regionHeight = maxY - minY
	local destination = destinationRegion or Rect.new(Vector2.zero, outputSize)
	local destinationMinX = destination.Min.X
	local destinationMinY = destination.Min.Y
	local destinationMaxX = destination.Max.X
	local destinationMaxY = destination.Max.Y

	if
		not finite(destinationMinX)
		or not finite(destinationMinY)
		or not finite(destinationMaxX)
		or not finite(destinationMaxY)
		or destinationMinX % 1 ~= 0
		or destinationMinY % 1 ~= 0
		or destinationMaxX % 1 ~= 0
		or destinationMaxY % 1 ~= 0
		or destinationMinX < 0
		or destinationMinY < 0
		or destinationMaxX > outputWidth
		or destinationMaxY > outputHeight
		or destinationMaxX <= destinationMinX
		or destinationMaxY <= destinationMinY
	then
		error("destination region must be an integer rectangle inside the output", 2)
	end

	local destinationWidth = destinationMaxX - destinationMinX
	local destinationHeight = destinationMaxY - destinationMinY

	local integerCopy = minX % 1 == 0
		and minY % 1 == 0
		and regionWidth == destinationWidth
		and regionHeight == destinationHeight
	if integerCopy then
		local bytesPerRow = destinationWidth * 4
		for row = 0, destinationHeight - 1 do
			local sourceOffset = ((minY + row) * sourceWidth + minX) * 4
			local destinationOffset = ((destinationMinY + row) * outputWidth + destinationMinX) * 4
			buffer.copy(output, destinationOffset, source.pixels, sourceOffset, bytesPerRow)
		end
		return output
	end

	for destinationY = 0, destinationHeight - 1 do
		local sourceY = minY + (destinationY + 0.5) * regionHeight / destinationHeight - 0.5
		sourceY = math.clamp(sourceY, 0, sourceHeight - 1)
		local y0 = math.floor(sourceY)
		local y1 = math.min(y0 + 1, sourceHeight - 1)
		local yWeight = sourceY - y0
		local outputY = destinationMinY + destinationY

		for destinationX = 0, destinationWidth - 1 do
			local sourceX = minX + (destinationX + 0.5) * regionWidth / destinationWidth - 0.5
			sourceX = math.clamp(sourceX, 0, sourceWidth - 1)
			local x0 = math.floor(sourceX)
			local x1 = math.min(x0 + 1, sourceWidth - 1)
			local xWeight = sourceX - x0
			local outputX = destinationMinX + destinationX
			local topLeft = (y0 * sourceWidth + x0) * 4
			local topRight = (y0 * sourceWidth + x1) * 4
			local bottomLeft = (y1 * sourceWidth + x0) * 4
			local bottomRight = (y1 * sourceWidth + x1) * 4
			local outputOffset = (outputY * outputWidth + outputX) * 4

			local inverseX = 1 - xWeight
			local inverseY = 1 - yWeight
			local topLeftWeight = inverseX * inverseY
			local topRightWeight = xWeight * inverseY
			local bottomLeftWeight = inverseX * yWeight
			local bottomRightWeight = xWeight * yWeight
			local topLeftAlpha = buffer.readu8(source.pixels, topLeft + 3) / 255
			local topRightAlpha = buffer.readu8(source.pixels, topRight + 3) / 255
			local bottomLeftAlpha = buffer.readu8(source.pixels, bottomLeft + 3) / 255
			local bottomRightAlpha = buffer.readu8(source.pixels, bottomRight + 3) / 255
			local outputAlpha = topLeftAlpha * topLeftWeight
				+ topRightAlpha * topRightWeight
				+ bottomLeftAlpha * bottomLeftWeight
				+ bottomRightAlpha * bottomRightWeight

			for channel = 0, 2 do
				local premultiplied = buffer.readu8(source.pixels, topLeft + channel) * topLeftAlpha * topLeftWeight
					+ buffer.readu8(source.pixels, topRight + channel) * topRightAlpha * topRightWeight
					+ buffer.readu8(source.pixels, bottomLeft + channel) * bottomLeftAlpha * bottomLeftWeight
					+ buffer.readu8(source.pixels, bottomRight + channel) * bottomRightAlpha * bottomRightWeight
				local value = if outputAlpha > 1e-8 then premultiplied / outputAlpha else 0
				buffer.writeu8(output, outputOffset + channel, math.floor(math.clamp(value, 0, 255) + 0.5))
			end
			buffer.writeu8(output, outputOffset + 3, math.floor(math.clamp(outputAlpha * 255, 0, 255) + 0.5))
		end
	end

	return output
end

local function containedDestination(sourceSize: Vector2, outputSize: Vector2): Rect
	local scale = math.min(outputSize.X / sourceSize.X, outputSize.Y / sourceSize.Y)
	local width = math.clamp(math.floor(sourceSize.X * scale + 0.5), 1, outputSize.X)
	local height = math.clamp(math.floor(sourceSize.Y * scale + 0.5), 1, outputSize.Y)
	local x = math.floor((outputSize.X - width) * 0.5)
	local y = math.floor((outputSize.Y - height) * 0.5)
	return Rect.new(Vector2.new(x, y), Vector2.new(x + width, y + height))
end

local function recoverAlpha(dark: RawImage, light: RawImage, exactBackdropEndpoints: boolean?): RawImage
	if dark.size ~= light.size then
		error("background passes returned different dimensions", 2)
	end

	local width = dark.size.X
	local height = dark.size.Y
	local byteLength = width * height * 4
	local spans: { number } = if exactBackdropEndpoints then { 255, 255, 255 } else { -1, -1, -1 }
	local baselineSums: { number } = { 0, 0, 0 }
	local baselineCounts: { number } = if exactBackdropEndpoints then { 1, 1, 1 } else { 0, 0, 0 }

	-- The renderer can remap both backdrop endpoints through exposure and color
	-- effects. Find the observed endpoint separation for each channel instead of
	-- assuming that the two passes differ by the full byte range. If several
	-- pixels share the maximum span, average their dark endpoint to avoid choosing
	-- a single noisy sample as the baseline.
	if not exactBackdropEndpoints then
		for offset = 0, byteLength - 4, 4 do
			for channel = 0, 2 do
				local darkValue = buffer.readu8(dark.pixels, offset + channel)
				local lightValue = buffer.readu8(light.pixels, offset + channel)
				local span = math.max(0, lightValue - darkValue)
				local index = channel + 1
				if span > spans[index] then
					spans[index] = span
					baselineSums[index] = darkValue
					baselineCounts[index] = 1
				elseif span == spans[index] then
					baselineSums[index] += darkValue
					baselineCounts[index] += 1
				end
			end
		end
	end

	for channel = 1, 3 do
		if spans[channel] < 4 or baselineCounts[channel] < 1 then
			error("background passes do not contain enough contrast to recover alpha", 2)
		end
	end
	local baselines: { number } = {
		baselineSums[1] / baselineCounts[1],
		baselineSums[2] / baselineCounts[2],
		baselineSums[3] / baselineCounts[3],
	}

	local output = buffer.create(width * height * 4)
	local noiseFloor = if exactBackdropEndpoints then UI_ALPHA_NOISE_FLOOR else ALPHA_NOISE_FLOOR
	for offset = 0, byteLength - 4, 4 do
		local darkR = buffer.readu8(dark.pixels, offset)
		local darkG = buffer.readu8(dark.pixels, offset + 1)
		local darkB = buffer.readu8(dark.pixels, offset + 2)
		local deltaR = math.max(0, buffer.readu8(light.pixels, offset) - darkR)
		local deltaG = math.max(0, buffer.readu8(light.pixels, offset + 1) - darkG)
		local deltaB = math.max(0, buffer.readu8(light.pixels, offset + 2) - darkB)
		local emptyBackdrop = deltaR >= spans[1] - 1 and deltaG >= spans[2] - 1 and deltaB >= spans[3] - 1
		local backgroundShare = (
			math.clamp(deltaR / spans[1], 0, 1)
			+ math.clamp(deltaG / spans[2], 0, 1)
			+ math.clamp(deltaB / spans[3], 0, 1)
		) / 3
		local coverage = if emptyBackdrop then 0 else math.clamp(1 - backgroundShare, 0, 1)
		if coverage <= noiseFloor then
			coverage = 0
		end
		local alpha = math.floor(coverage * 255 + 0.5)

		if alpha > 0 then
			local encodedCoverage = alpha / 255
			local recoveredR = (darkR - baselines[1] * (1 - encodedCoverage)) / encodedCoverage
			local recoveredG = (darkG - baselines[2] * (1 - encodedCoverage)) / encodedCoverage
			local recoveredB = (darkB - baselines[3] * (1 - encodedCoverage)) / encodedCoverage
			buffer.writeu8(output, offset, math.floor(math.clamp(recoveredR, 0, 255) + 0.5))
			buffer.writeu8(output, offset + 1, math.floor(math.clamp(recoveredG, 0, 255) + 0.5))
			buffer.writeu8(output, offset + 2, math.floor(math.clamp(recoveredB, 0, 255) + 0.5))
		end
		buffer.writeu8(output, offset + 3, alpha)
	end

	return { size = dark.size, pixels = output }
end

local function alphaBounds(image: RawImage, deadline: number): Rect
	local width = image.size.X
	local height = image.size.Y
	local minimumX = width
	local minimumY = height
	local maximumX = -1
	local maximumY = -1

	for y = 0, height - 1 do
		if y % 64 == 0 then
			assertTime(deadline, "UI target bounds")
		end
		for x = 0, width - 1 do
			if buffer.readu8(image.pixels, (y * width + x) * 4 + 3) > 0 then
				minimumX = math.min(minimumX, x)
				minimumY = math.min(minimumY, y)
				maximumX = math.max(maximumX, x)
				maximumY = math.max(maximumY, y)
			end
		end
	end

	if maximumX < minimumX or maximumY < minimumY then
		error("UI target rendered no visible pixels", 2)
	end
	return Rect.new(minimumX, minimumY, maximumX + 1, maximumY + 1)
end

local function bleedTransparentRGB(pixels: buffer, size: Vector2)
	local width = size.X
	local height = size.Y
	local coverage = buffer.create(width * height)
	for pixel = 0, width * height - 1 do
		if buffer.readu8(pixels, pixel * 4 + 3) > 0 then
			buffer.writeu8(coverage, pixel, 1)
		end
	end

	local function copyColor(destination: number, source: number)
		local destinationOffset = destination * 4
		local sourceOffset = source * 4
		buffer.writeu8(pixels, destinationOffset, buffer.readu8(pixels, sourceOffset))
		buffer.writeu8(pixels, destinationOffset + 1, buffer.readu8(pixels, sourceOffset + 1))
		buffer.writeu8(pixels, destinationOffset + 2, buffer.readu8(pixels, sourceOffset + 2))
		buffer.writeu8(coverage, destination, 1)
	end

	for y = 0, height - 1 do
		for x = 1, width - 1 do
			local pixel = y * width + x
			if buffer.readu8(coverage, pixel) == 0 and buffer.readu8(coverage, pixel - 1) ~= 0 then
				copyColor(pixel, pixel - 1)
			end
		end
		for x = width - 2, 0, -1 do
			local pixel = y * width + x
			if buffer.readu8(coverage, pixel) == 0 and buffer.readu8(coverage, pixel + 1) ~= 0 then
				copyColor(pixel, pixel + 1)
			end
		end
	end

	for y = 1, height - 1 do
		for x = 0, width - 1 do
			local pixel = y * width + x
			if buffer.readu8(coverage, pixel) == 0 and buffer.readu8(coverage, pixel - width) ~= 0 then
				copyColor(pixel, pixel - width)
			end
		end
	end
	for y = height - 2, 0, -1 do
		for x = 0, width - 1 do
			local pixel = y * width + x
			if buffer.readu8(coverage, pixel) == 0 and buffer.readu8(coverage, pixel + width) ~= 0 then
				copyColor(pixel, pixel + width)
			end
		end
	end
end

local function chooseOutput(sourceSize: Vector2, region: Rect?, requested: Vector2?): (Rect, Vector2)
	local selected = region or Rect.new(Vector2.zero, sourceSize)
	local fallback = Vector2.new(math.floor(selected.Width + 0.5), math.floor(selected.Height + 0.5))
	local output = checkedSize(requested, fallback) :: Vector2
	return selected, output
end

local function createBackdropSurface(
	camera: Camera,
	register: CleanupRegistrar,
	distance: number,
	width: number,
	height: number,
	alwaysOnTop: boolean
): Frame
	local part = Instance.new("Part")
	register(function()
		part:Destroy()
	end)
	part.Name = "RoSyncCaptureBackdrop"
	part.Anchored = true
	part.CanCollide = false
	part.CanQuery = false
	part.CanTouch = false
	part.CastShadow = false
	part.Transparency = 1
	local physicalWidth = math.min(width, 2_048)
	local physicalHeight = math.min(height, 2_048)
	part.Size = Vector3.new(physicalWidth, physicalHeight, 0.1)
	part.CFrame = CFrame.lookAt(
		camera.CFrame.Position + camera.CFrame.LookVector * distance,
		camera.CFrame.Position,
		camera.CFrame.UpVector
	)

	local mesh = Instance.new("SpecialMesh")
	mesh.MeshType = Enum.MeshType.Brick
	mesh.Scale = Vector3.new(width / physicalWidth, height / physicalHeight, 1)
	mesh.Parent = part

	local surface = Instance.new("SurfaceGui")
	surface.Name = "BackdropSurface"
	surface.Adornee = part
	surface.Face = Enum.NormalId.Front
	surface.AlwaysOnTop = alwaysOnTop
	surface.LightInfluence = 0
	surface.MaxDistance = 0
	surface.SizingMode = Enum.SurfaceGuiSizingMode.FixedSize
	surface.CanvasSize = Vector2.new(64, 64)
	surface.Parent = part

	local color = Instance.new("Frame")
	color.Name = "Color"
	color.BorderSizePixel = 0
	color.Size = UDim2.fromScale(1, 1)
	color.BackgroundColor3 = Color3.new(0, 0, 0)
	color.Parent = surface

	part.Parent = Workspace
	return color
end

local function createBackdrop(camera: Camera, register: CleanupRegistrar): Frame
	local farthest = 256
	local viewport = camera.ViewportSize
	local aspect = viewport.X / math.max(1, viewport.Y)
	local tangentY = math.tan(math.rad(camera.FieldOfView * 0.5))
	local tangentX = tangentY * aspect
	for _, item in Workspace:GetDescendants() do
		if item:IsA("BasePart") then
			local relative = camera.CFrame:PointToObjectSpace(item.Position)
			local radius = item.Size.Magnitude * 0.5
			local depth = -relative.Z
			local projectedDepth = math.max(depth, 0)
			local intersectsView = depth + radius > 0
				and math.abs(relative.X) <= projectedDepth * tangentX + radius
				and math.abs(relative.Y) <= projectedDepth * tangentY + radius
			if intersectsView then
				farthest = math.max(farthest, depth + radius)
			end
		end
	end

	local distance = math.clamp(farthest + math.max(32, farthest * 0.05), 256, 50_000)
	local height = 2 * distance * math.tan(math.rad(camera.FieldOfView * 0.5)) * 1.2
	local width = height * aspect * 1.2
	return createBackdropSurface(camera, register, distance, width, height, false)
end

local function createUIOnlyBackdrop(register: CleanupRegistrar): (Frame, ScreenGui)
	-- A bottom-most ScreenGui is composited by CaptureService in the same layer
	-- as the UI we want to preserve. Unlike a world SurfaceGui, it reliably
	-- covers every 3D pixel while remaining behind all ordinary ScreenGuis.
	-- Capturing this frame once in black and once in white gives exact backdrop
	-- endpoints, so the UI layer can be reconstructed as straight-alpha RGBA.
	local screen = Instance.new("ScreenGui")
	register(function()
		screen:Destroy()
	end)
	screen.Name = "RoSyncCaptureUIBackdrop"
	screen.Archivable = false
	screen.DisplayOrder = -2_147_483_648
	screen.IgnoreGuiInset = true
	screen.ResetOnSpawn = false
	screen.ZIndexBehavior = Enum.ZIndexBehavior.Global

	local color = Instance.new("Frame")
	color.Name = "Color"
	color.BorderSizePixel = 0
	color.Position = UDim2.fromScale(0, 0)
	color.Size = UDim2.fromScale(1, 1)
	color.BackgroundColor3 = Color3.new(0, 0, 0)
	color.ZIndex = 1
	color.Parent = screen

	-- Parent only after the hierarchy is complete so no partial backdrop can
	-- flash into a screenshot between construction and the first render step.
	screen.Parent = StarterGui
	return color, screen
end

local function cloneUiTree(target: Instance): Instance
	local changedArchivable: { Instance } = {}
	local candidates = target:GetDescendants()
	table.insert(candidates, target)
	for _, item in candidates do
		if not item.Archivable then
			local changed = pcall(function()
				item.Archivable = true
			end)
			if changed then
				table.insert(changedArchivable, item)
			end
		end
	end

	local result: Instance? = nil
	local ok, cloneError = pcall(function()
		result = target:Clone()
	end)
	for index = #changedArchivable, 1, -1 do
		local item = changedArchivable[index]
		pcall(function()
			item.Archivable = false
		end)
	end
	if not ok or result == nil then
		error("unable to clone UI target: " .. tostring(cloneError), 2)
	end

	local clone = result :: Instance
	local descendants = clone:GetDescendants()
	for index = #descendants, 1, -1 do
		local item = descendants[index]
		if item:IsA("LuaSourceContainer") then
			item:Destroy()
		end
	end
	return clone
end

local function ancestorUiScale(target: GuiObject, owner: ScreenGui): number
	local scale = 1
	local cursor = target.Parent
	while cursor ~= nil and cursor ~= owner do
		if cursor:IsA("GuiObject") then
			local scaler = cursor:FindFirstChildOfClass("UIScale")
			if scaler ~= nil and finite(scaler.Scale) and scaler.Scale > 0 then
				scale *= scaler.Scale
			end
		end
		cursor = cursor.Parent
	end
	return scale
end

local function uiClipRect(target: GuiObject, owner: ScreenGui, viewport: Vector2): Rect
	local minimum = Vector2.zero
	local maximum = viewport
	local cursor = target.Parent
	while cursor ~= nil and cursor ~= owner do
		if cursor:IsA("GuiObject") and cursor.ClipsDescendants then
			minimum = minimum:Max(cursor.AbsolutePosition)
			maximum = maximum:Min(cursor.AbsolutePosition + cursor.AbsoluteSize)
		end
		cursor = cursor.Parent
	end
	if maximum.X <= minimum.X or maximum.Y <= minimum.Y then
		error("UI target is completely clipped by its ancestors", 2)
	end
	return Rect.new(minimum, maximum)
end

local function copyNearestStyleLink(target: Instance, destination: Instance)
	local cursor = target.Parent
	while cursor ~= nil do
		local styleLink = cursor:FindFirstChildWhichIsA("StyleLink")
		if styleLink ~= nil then
			local ok, copy = pcall(function()
				return styleLink:Clone()
			end)
			if ok then
				copy.Parent = destination
			end
			return
		end
		cursor = cursor.Parent
	end
end

local function createUiTargetLayer(target: Instance, camera: Camera, register: CleanupRegistrar): ScreenGui
	if target.Parent == nil or not target:IsDescendantOf(game) then
		error("uiTarget must be an instance in the current DataModel", 2)
	end
	if not target:IsA("ScreenGui") and not target:IsA("GuiObject") then
		error("uiTarget must be a ScreenGui or GuiObject", 2)
	end

	if target:IsA("ScreenGui") then
		local clone = cloneUiTree(target)
		if not clone:IsA("ScreenGui") then
			clone:Destroy()
			error("unable to clone ScreenGui target", 2)
		end
		register(function()
			clone:Destroy()
		end)
		clone.Name = "RoSyncCaptureUITarget"
		clone.Archivable = false
		clone.DisplayOrder = 0
		clone.Enabled = true
		clone.ResetOnSpawn = false
		copyNearestStyleLink(target, clone)
		clone.Parent = StarterGui
		return clone
	end

	local guiTarget = target :: GuiObject
	local owner = guiTarget:FindFirstAncestorWhichIsA("ScreenGui")
	if owner == nil then
		error("a GuiObject uiTarget must be beneath a ScreenGui", 2)
	end
	local clone = cloneUiTree(guiTarget)
	if not clone:IsA("GuiObject") then
		clone:Destroy()
		error("unable to clone GuiObject target", 2)
	end
	register(function()
		clone:Destroy()
	end)

	local screen = Instance.new("ScreenGui")
	register(function()
		screen:Destroy()
	end)
	screen.Name = "RoSyncCaptureUITarget"
	screen.Archivable = false
	screen.DisplayOrder = 0
	screen.IgnoreGuiInset = true
	screen.ResetOnSpawn = false
	screen.ZIndexBehavior = owner.ZIndexBehavior

	local clip = uiClipRect(guiTarget, owner, camera.ViewportSize)
	local holder = Instance.new("Frame")
	holder.Name = "TargetClip"
	holder.BackgroundTransparency = 1
	holder.BorderSizePixel = 0
	holder.ClipsDescendants = true
	holder.Position = UDim2.fromOffset(clip.Min.X, clip.Min.Y)
	holder.Size = UDim2.fromOffset(clip.Width, clip.Height)
	holder.Parent = screen

	for _, item in clone:GetChildren() do
		if item:IsA("UIAspectRatioConstraint") or item:IsA("UISizeConstraint") then
			item:Destroy()
		end
	end
	local scale = ancestorUiScale(guiTarget, owner)
	local cloneScale = clone:FindFirstChildOfClass("UIScale")
	if cloneScale ~= nil and finite(cloneScale.Scale) and cloneScale.Scale > 0 then
		scale *= cloneScale.Scale
		cloneScale.Scale = scale
	elseif math.abs(scale - 1) > 1e-6 then
		local addedScale = Instance.new("UIScale")
		addedScale.Scale = scale
		addedScale.Parent = clone
	end
	scale = math.max(scale, 1e-6)
	clone.AnchorPoint = Vector2.zero
	clone.AutomaticSize = Enum.AutomaticSize.None
	clone.Visible = true
	clone.Position =
		UDim2.fromOffset(guiTarget.AbsolutePosition.X - clip.Min.X, guiTarget.AbsolutePosition.Y - clip.Min.Y)
	clone.Rotation = guiTarget.AbsoluteRotation
	clone.Size = UDim2.fromOffset(guiTarget.AbsoluteSize.X / scale, guiTarget.AbsoluteSize.Y / scale)
	clone.Parent = holder
	copyNearestStyleLink(guiTarget, holder)

	screen.Parent = StarterGui
	return screen
end

local function uiRoots(): { Instance }
	local roots: { Instance } = { CoreGui, StarterGui, Workspace }
	local player: Player? = (Players :: any).LocalPlayer
	if player ~= nil then
		local playerGui = player:FindFirstChildOfClass("PlayerGui")
		if playerGui ~= nil then
			table.insert(roots, playerGui)
		end
	end
	return roots
end

local function hideOtherLayerCollectors(exclusions: { Instance }, register: CleanupRegistrar)
	local states: { { item: LayerCollector, enabled: boolean } } = {}
	local seen: { [Instance]: boolean } = {}
	local roots = uiRoots()

	local function isExcluded(item: Instance): boolean
		for _, exclusion in exclusions do
			if item == exclusion or item:IsDescendantOf(exclusion) then
				return true
			end
		end
		return false
	end

	local function hide(item: Instance)
		if seen[item] or not item:IsA("LayerCollector") or isExcluded(item) then
			return
		end
		seen[item] = true
		local readOk, enabled = pcall(function()
			return item.Enabled
		end)
		if readOk and enabled then
			local writeOk = pcall(function()
				item.Enabled = false
			end)
			if writeOk then
				table.insert(states, { item = item, enabled = not not enabled })
			end
		end
	end

	register(function()
		for index = #states, 1, -1 do
			local state = states[index]
			if state.item.Parent ~= nil then
				pcall(function()
					state.item.Enabled = state.enabled
				end)
			end
		end
	end)

	local connections: { RBXScriptConnection } = {}
	for _, root in roots do
		table.insert(connections, root.DescendantAdded:Connect(hide))
		local ok, descendants = pcall(function()
			return root:GetDescendants()
		end)
		if ok then
			for _, item in descendants do
				hide(item)
			end
		end
	end
	register(function()
		for _, connection in connections do
			connection:Disconnect()
		end
	end)
end

local function captureViewInternal(options: CaptureViewOptions, register: CleanupRegistrar): Image
	local camera: Camera? = (Workspace :: any).CurrentCamera
	if camera == nil then
		error("Workspace has no current camera", 2)
	end

	local background = checkedBackground(options.background)
	local uiMode = checkedUiMode(options.uiMode, options.hideUI)
	if uiMode == "only" then
		if background == "scene" then
			error('uiMode "only" requires a transparent background', 2)
		end
	end
	local uiTargetLayer: ScreenGui? = nil
	if options.uiTarget ~= nil then
		if uiMode ~= "only" then
			error('uiTarget requires uiMode "only"', 2)
		end
		if typeof(options.uiTarget) ~= "Instance" then
			error("uiTarget must be an Instance", 2)
		end
		uiTargetLayer = createUiTargetLayer(options.uiTarget, camera, register)
	end
	local deadline, delaySeconds = deadlineFrom(options :: any)
	if uiMode == "none" then
		hideScreenInterfaces(register)
	end

	local raw: RawImage
	if background == "scene" then
		raw = takeScreenshot(deadline, delaySeconds)
	else
		local backdropColor: Frame
		if uiMode == "only" then
			local backdropScreen: ScreenGui
			backdropColor, backdropScreen = createUIOnlyBackdrop(register)
			if uiTargetLayer ~= nil then
				hideOtherLayerCollectors({ uiTargetLayer, backdropScreen }, register)
			end
		else
			backdropColor = createBackdrop(camera, register)
		end
		backdropColor.BackgroundColor3 = Color3.new(0, 0, 0)
		local dark = takeScreenshot(deadline, delaySeconds)
		backdropColor.BackgroundColor3 = Color3.new(1, 1, 1)
		local light = takeScreenshot(deadline, math.min(delaySeconds, 0.1))
		raw = recoverAlpha(dark, light, uiMode == "only")
	end

	local selectedRegion = options.region
	local regionSource = if selectedRegion ~= nil then "explicit" else "full"
	if selectedRegion == nil and uiTargetLayer ~= nil then
		selectedRegion = alphaBounds(raw, deadline)
		regionSource = "target-alpha"
	end
	local region, outputSize = chooseOutput(raw.size, selectedRegion, options.size)
	local destination = if uiMode == "only"
			and options.region == nil
			and options.size ~= nil
		then containedDestination(Vector2.new(region.Width, region.Height), outputSize)
		else nil
	local output = resample(raw, region, outputSize, destination)
	if background == "transparent" and options.alphaBleed == true then
		bleedTransparentRGB(output, outputSize)
	end
	assertTime(deadline, "capture processing")
	return {
		size = outputSize,
		buffer = output,
		region = region,
		fullSize = raw.size,
		regionSource = regionSource,
	}
end

function NativePhoto.captureView(options: CaptureViewOptions?): Image
	local supplied = options or ({} :: CaptureViewOptions)
	return withCleanup(function(register)
		return captureViewInternal(supplied, register)
	end)
end

local function boundsOf(subject: Instance): (CFrame, Vector3)
	if subject:IsA("BasePart") then
		return subject.CFrame, subject.Size
	end
	if subject:IsA("Model") then
		return subject:GetBoundingBox()
	end

	local minimum = Vector3.new(math.huge, math.huge, math.huge)
	local maximum = Vector3.new(-math.huge, -math.huge, -math.huge)
	local found = false
	for _, item in subject:GetDescendants() do
		if item:IsA("BasePart") then
			found = true
			local half = item.Size * 0.5
			for _, x in { -1, 1 } do
				for _, y in { -1, 1 } do
					for _, z in { -1, 1 } do
						local point = (item.CFrame * CFrame.new(half.X * x, half.Y * y, half.Z * z)).Position
						minimum = Vector3.new(
							math.min(minimum.X, point.X),
							math.min(minimum.Y, point.Y),
							math.min(minimum.Z, point.Z)
						)
						maximum = Vector3.new(
							math.max(maximum.X, point.X),
							math.max(maximum.Y, point.Y),
							math.max(maximum.Z, point.Z)
						)
					end
				end
			end
		end
	end
	if not found then
		error("subject contains no renderable parts", 2)
	end

	local size = maximum - minimum
	return CFrame.new((minimum + maximum) * 0.5), size
end

local function cameraDirection(view: string?, supplied: Vector3?): Vector3
	if supplied ~= nil then
		if typeof(supplied) ~= "Vector3" or supplied.Magnitude < 1e-5 then
			error("direction must be a non-zero Vector3", 3)
		end
		return supplied.Unit
	end

	local name = string.lower(view or "isometric")
	local directions: { [string]: Vector3 } = {
		front = Vector3.new(0, 0, -1),
		back = Vector3.new(0, 0, 1),
		left = Vector3.new(-1, 0, 0),
		right = Vector3.new(1, 0, 0),
		top = Vector3.new(0, 1, 0),
		bottom = Vector3.new(0, -1, 0),
		isometric = Vector3.new(1, 0.75, -1),
		iso = Vector3.new(1, 0.75, -1),
	}
	local direction: Vector3? = (directions :: any)[name]
	if direction == nil then
		error("view must be front, back, left, right, top, bottom, or isometric", 3)
	end
	return direction.Unit
end

local function frameCamera(
	camera: Camera,
	boundsCFrame: CFrame,
	boundsSize: Vector3,
	direction: Vector3,
	padding: number,
	aspect: number,
	fieldOfView: number
)
	local center = boundsCFrame.Position
	local upHint = if math.abs(direction:Dot(Vector3.yAxis)) > 0.98 then Vector3.zAxis else Vector3.yAxis
	local basis = CFrame.lookAt(center + direction, center, upHint)
	local tangentY = math.tan(math.rad(fieldOfView * 0.5))
	local tangentX = tangentY * aspect
	local required = 0
	local half = boundsSize * 0.5

	for _, x in { -1, 1 } do
		for _, y in { -1, 1 } do
			for _, z in { -1, 1 } do
				local point = (boundsCFrame * CFrame.new(half.X * x, half.Y * y, half.Z * z)).Position
				local offset = point - center
				local towardCamera = offset:Dot(direction)
				local horizontal = math.abs(offset:Dot(basis.RightVector)) / tangentX
				local vertical = math.abs(offset:Dot(basis.UpVector)) / tangentY
				required = math.max(required, towardCamera + horizontal, towardCamera + vertical)
			end
		end
	end

	local distance = math.max(required * padding, boundsSize.Magnitude * 0.05, 0.1)
	camera.FieldOfView = fieldOfView
	camera.CFrame = CFrame.lookAt(center + direction * distance, center, upHint)
end

local function cloneForWorld(subject: Instance, register: CleanupRegistrar): Instance
	local previousArchivable = subject.Archivable
	if not previousArchivable then
		subject.Archivable = true
	end
	local ok, result = pcall(function()
		return subject:Clone()
	end)
	if not previousArchivable then
		subject.Archivable = false
	end
	if not ok then
		error("unable to clone subject: " .. tostring(result), 2)
	end

	local clone = result :: Instance
	register(function()
		clone:Destroy()
	end)
	if clone:IsA("LuaSourceContainer") then
		error("a script container cannot be rendered as the subject root", 2)
	end
	local candidates = clone:GetDescendants()
	table.insert(candidates, clone)
	for _, item in candidates do
		if item:IsA("BasePart") then
			item.Anchored = true
			item.CanCollide = false
			item.CanQuery = false
			item.CanTouch = false
			item.AssemblyLinearVelocity = Vector3.zero
			item.AssemblyAngularVelocity = Vector3.zero
		end
	end
	for index = #candidates, 1, -1 do
		local item = candidates[index]
		if item:IsA("LuaSourceContainer") then
			item:Destroy()
		end
	end
	return clone
end

local function remoteStageCenter(boundsSize: Vector3, direction: Vector3): Vector3
	local lowest = 0
	local highest = 0
	local horizontalReach = 0
	for _, item in Workspace:GetDescendants() do
		if item:IsA("BasePart") then
			local radius = item.Size.Magnitude * 0.5
			lowest = math.min(lowest, item.Position.Y - radius)
			highest = math.max(highest, item.Position.Y + radius)
			horizontalReach =
				math.max(horizontalReach, Vector2.new(item.Position.X, item.Position.Z).Magnitude + radius)
		end
	end

	local clearance = math.max(10_000, boundsSize.Magnitude * 8, horizontalReach * 2.2)
	local y = if direction.Y > 0.05 then lowest - clearance else highest + clearance
	return Vector3.new(0, y, 0)
end

local function moveCenterTo(subject: Instance, target: Vector3)
	local boundsCFrame = boundsOf(subject)
	local translation = target - boundsCFrame.Position
	local candidates = subject:GetDescendants()
	if subject:IsA("BasePart") then
		table.insert(candidates, subject)
	end
	for _, item in candidates do
		if item:IsA("BasePart") then
			item.CFrame = CFrame.new(translation) * item.CFrame
		end
	end
end

local function centerCrop(size: Vector2, aspect: number): Rect
	local sourceAspect = size.X / size.Y
	if sourceAspect > aspect then
		local width = size.Y * aspect
		local x = (size.X - width) * 0.5
		return Rect.new(x, 0, x + width, size.Y)
	end
	local height = size.X / aspect
	local y = (size.Y - height) * 0.5
	return Rect.new(0, y, size.X, y + height)
end

local function renderIsolated(subject: Instance, options: RenderInstanceOptions, register: CleanupRegistrar): Image
	local outputSize = checkedSize(options.size, DEFAULT_SIZE) :: Vector2
	local background = checkedBackground(options.background)
	local uiMode = checkedUiMode(options.uiMode, options.hideUI)
	if uiMode == "only" then
		error('uiMode "only" captures the current viewport and cannot render a focused instance', 2)
	end
	local deadline, delaySeconds = deadlineFrom(options :: any)
	local padding = checkedNumber(options.padding, 1.15, 0.1, 10, "padding")
	local fieldOfView = checkedNumber(options.fieldOfView, 35, 1, 120, "fieldOfView")
	local cameraCFrame = checkedCameraCFrame(options.cameraCFrame)
	local direction: Vector3? = if cameraCFrame == nil then cameraDirection(options.view, options.direction) else nil
	local camera: Camera? = (Workspace :: any).CurrentCamera
	if camera == nil then
		error("Workspace has no current camera", 2)
	end

	if uiMode == "none" then
		hideScreenInterfaces(register)
	end

	local oldCFrame = camera.CFrame
	local oldFocus = camera.Focus
	local oldFieldOfView = camera.FieldOfView
	local oldCameraType = camera.CameraType
	register(function()
		camera.CFrame = oldCFrame
		camera.Focus = oldFocus
		camera.FieldOfView = oldFieldOfView
		camera.CameraType = oldCameraType
	end)

	local clone = cloneForWorld(subject, register)
	local initialBounds, initialSize = boundsOf(clone)
	local relativeCamera: CFrame? = nil
	if cameraCFrame ~= nil then
		relativeCamera = initialBounds:ToObjectSpace(cameraCFrame)
		local offset = cameraCFrame.Position - initialBounds.Position
		direction = if offset.Magnitude > 1e-5 then offset.Unit else -cameraCFrame.LookVector
	end
	moveCenterTo(clone, remoteStageCenter(initialSize, direction :: Vector3))
	clone.Name = "RoSyncNativeTarget"
	clone.Parent = Workspace

	local boundsCFrame, boundsSize = boundsOf(clone)
	local liveAspect = camera.ViewportSize.X / math.max(1, camera.ViewportSize.Y)
	local outputAspect = outputSize.X / outputSize.Y
	camera.CameraType = Enum.CameraType.Scriptable
	if cameraCFrame ~= nil then
		camera.FieldOfView = fieldOfView
		camera.CFrame = boundsCFrame * (relativeCamera :: CFrame)
	else
		frameCamera(
			camera,
			boundsCFrame,
			boundsSize,
			direction :: Vector3,
			padding,
			math.min(liveAspect, outputAspect),
			fieldOfView
		)
	end
	camera.Focus = boundsCFrame

	local raw: RawImage
	if background == "transparent" then
		local backdropColor = createBackdrop(camera, register)
		backdropColor.BackgroundColor3 = Color3.new(0, 0, 0)
		local dark = takeScreenshot(deadline, delaySeconds)
		backdropColor.BackgroundColor3 = Color3.new(1, 1, 1)
		local light = takeScreenshot(deadline, math.min(delaySeconds, 0.1))
		raw = recoverAlpha(dark, light)
	else
		raw = takeScreenshot(deadline, delaySeconds)
	end

	local crop = centerCrop(raw.size, outputAspect)
	local output = resample(raw, crop, outputSize)
	if background == "transparent" and options.alphaBleed == true then
		bleedTransparentRGB(output, outputSize)
	end
	assertTime(deadline, "instance render processing")
	return {
		size = outputSize,
		buffer = output,
		region = crop,
		fullSize = raw.size,
		regionSource = "framed",
	}
end

local function renderInScene(subject: Instance, options: RenderInstanceOptions, register: CleanupRegistrar): Image
	if not subject:IsDescendantOf(Workspace) then
		error("isolate=false requires a subject inside Workspace", 2)
	end
	local camera: Camera? = (Workspace :: any).CurrentCamera
	if camera == nil then
		error("Workspace has no current camera", 2)
	end

	local outputSize = checkedSize(options.size, DEFAULT_SIZE) :: Vector2
	local background = checkedBackground(options.background)
	local uiMode = checkedUiMode(options.uiMode, options.hideUI)
	if uiMode == "only" then
		error('uiMode "only" captures the current viewport and cannot render a focused instance', 2)
	end
	local fieldOfView = checkedNumber(options.fieldOfView, 35, 1, 120, "fieldOfView")
	local padding = checkedNumber(options.padding, 1.15, 0.1, 10, "padding")
	local cameraCFrame = checkedCameraCFrame(options.cameraCFrame)
	local direction = if cameraCFrame == nil then cameraDirection(options.view, options.direction) else nil
	local oldCFrame = camera.CFrame
	local oldFocus = camera.Focus
	local oldFieldOfView = camera.FieldOfView
	local oldCameraType = camera.CameraType
	register(function()
		camera.CFrame = oldCFrame
		camera.Focus = oldFocus
		camera.FieldOfView = oldFieldOfView
		camera.CameraType = oldCameraType
	end)

	local boundsCFrame, boundsSize = boundsOf(subject)
	camera.CameraType = Enum.CameraType.Scriptable
	if cameraCFrame ~= nil then
		camera.FieldOfView = fieldOfView
		camera.CFrame = cameraCFrame
	else
		frameCamera(
			camera,
			boundsCFrame,
			boundsSize,
			direction :: Vector3,
			padding,
			outputSize.X / outputSize.Y,
			fieldOfView
		)
	end
	camera.Focus = boundsCFrame

	local captureOptions: CaptureViewOptions = {
		background = background :: "transparent" | "scene",
		delay = options.delay,
		timeout = options.timeout,
		alphaBleed = false,
		uiMode = uiMode :: UiMode,
		hideUI = uiMode == "none",
	}
	local captured = captureViewInternal(captureOptions, register)
	local raw: RawImage = { size = captured.size, pixels = captured.buffer }
	local crop = centerCrop(raw.size, outputSize.X / outputSize.Y)
	local output = resample(raw, crop, outputSize)
	if background == "transparent" and options.alphaBleed == true then
		bleedTransparentRGB(output, outputSize)
	end
	return {
		size = outputSize,
		buffer = output,
		region = crop,
		fullSize = raw.size,
		regionSource = "framed",
	}
end

function NativePhoto.renderInstance(subject: Instance, options: RenderInstanceOptions?): Image
	if typeof(subject) ~= "Instance" then
		error("subject must be an Instance", 2)
	end
	local supplied = options or ({} :: RenderInstanceOptions)
	return withCleanup(function(register)
		if supplied.isolate == false then
			return renderInScene(subject, supplied, register)
		end
		return renderIsolated(subject, supplied, register)
	end)
end

return NativePhoto
