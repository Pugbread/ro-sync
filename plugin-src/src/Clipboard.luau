--!strict

-- Native cross-Studio clipboard support. Payloads are opaque `.rbxm` buffers
-- produced and consumed by Roblox's SerializationService; Ro Sync only moves
-- bounded, hash-checked chunks between connected Studio sessions.

local Clipboard = {}

local Selection: any = game:GetService("Selection")
local ChangeHistoryService: any = game:GetService("ChangeHistoryService")
local EncodingService = game:GetService("EncodingService")
local ScriptEditorService: any = game:GetService("ScriptEditorService")

local MAX_ROOTS = 256
local MAX_BYTES = 128 * 1024 * 1024
local CHUNK_BYTES = 384 * 1024
local DEADLINE_MARGIN_SECONDS = 1

local function clipboardDeadline(timeoutSeconds: any, receivedAt: any): number
	local requested = math.clamp(tonumber(timeoutSeconds) or 120, 1, 300)
	local now = os.clock()
	local started = tonumber(receivedAt) or now
	if started ~= started or started > now or now - started > 310 then
		started = now
	end
	return started + math.max(0.1, requested - DEADLINE_MARGIN_SECONDS)
end

local function checkDeadline(deadline: number, phase: string)
	if os.clock() >= deadline then
		error("clipboard " .. phase .. " timed out")
	end
end

local function sha256BufferHex(bytes: buffer): string
	local digest = EncodingService:ComputeBufferHash(bytes, Enum.HashAlgorithm.Sha256)
	local parts = table.create(buffer.len(digest))
	for index = 0, buffer.len(digest) - 1 do
		parts[index + 1] = string.format("%02x", buffer.readu8(digest, index))
	end
	return table.concat(parts)
end

local function errorDetail(response: any): string
	if type(response) == "table" then
		local err = response.error
		if type(err) == "table" then
			return tostring(err.message or err.code or "request rejected")
		end
		return tostring(err or "request rejected")
	end
	return tostring(response)
end

local function setArchivableTree(root: Instance, value: boolean, changed: { Instance }?)
	local items = { root }
	for _, descendant in ipairs(root:GetDescendants()) do
		table.insert(items, descendant)
	end
	for _, inst in ipairs(items) do
		local ok, current = pcall(function()
			return inst.Archivable
		end)
		if ok and current ~= value then
			local wrote = pcall(function()
				inst.Archivable = value
			end)
			if wrote and changed then
				table.insert(changed, inst)
			end
		end
	end
end

local function restoreArchivable(changed: { Instance })
	for index = #changed, 1, -1 do
		local inst = changed[index]
		pcall(function()
			inst.Archivable = false
		end)
	end
end

local function destroyRoots(roots: { Instance })
	for _, root in ipairs(roots) do
		pcall(function()
			root:Destroy()
		end)
	end
end

local function patchVisibleScriptSources(original: Instance, detached: Instance, readScriptSource: (Instance) -> string)
	if original:IsA("LuaSourceContainer") then
		if not detached:IsA("LuaSourceContainer") then
			error("serialized script shape changed while preparing clipboard")
		end
		local source = readScriptSource(original)
		local applied = pcall(function()
			ScriptEditorService:UpdateSourceAsync(detached, function(_oldSource)
				return source
			end)
		end)
		if not applied then
			applied = pcall(function()
				(detached :: any).Source = source
			end)
		end
		if not applied then
			error("could not preserve the visible editor source for " .. original:GetFullName())
		end
	end
	local originalChildren = original:GetChildren()
	local detachedChildren = detached:GetChildren()
	if #originalChildren ~= #detachedChildren then
		error("serialized instance shape changed while preparing clipboard")
	end
	for index, originalChild in ipairs(originalChildren) do
		local detachedChild = detachedChildren[index]
		if originalChild.ClassName ~= detachedChild.ClassName or originalChild.Name ~= detachedChild.Name then
			error("serialized child order changed while preparing clipboard")
		end
		patchVisibleScriptSources(originalChild, detachedChild, readScriptSource)
	end
end

local function normalizeRoots(paths: any, resolvePath: (string) -> Instance?): { Instance }
	local requested = {}
	if type(paths) == "table" and #paths > 0 then
		if #paths > MAX_ROOTS then
			error(string.format("copy supports at most %d root paths", MAX_ROOTS))
		end
		for index, path in ipairs(paths) do
			local inst = resolvePath(tostring(path))
			if not inst then
				error(string.format("path not found (index %d): %s", index, tostring(path)))
			end
			table.insert(requested, inst)
		end
	else
		requested = Selection:Get()
	end
	if #requested == 0 then
		error("Studio selection is empty")
	end
	if #requested > MAX_ROOTS then
		error(string.format("copy supports at most %d selected roots", MAX_ROOTS))
	end

	local selected = {}
	for _, inst in ipairs(requested) do
		if inst == game or inst.Parent == game then
			error("DataModel services cannot be copied; copy their children instead")
		end
		if not inst:IsDescendantOf(game) or inst.Parent == nil then
			error("only parented edit-mode DataModel instances can be copied")
		end
		selected[inst] = true
	end
	local roots = {}
	local inserted = {}
	for _, inst in ipairs(requested) do
		if not inserted[inst] then
			local ancestor = inst.Parent
			local nested = false
			while ancestor and ancestor ~= game do
				if selected[ancestor] then
					nested = true
					break
				end
				ancestor = ancestor.Parent
			end
			if not nested then
				inserted[inst] = true
				table.insert(roots, inst)
			end
		end
	end
	if #roots == 0 then
		error("Studio selection did not contain a copyable root")
	end
	return roots
end

local function routeForInstance(inst: Instance): { any }
	local route = {}
	local current: Instance? = inst
	while current and current ~= game do
		local parent = current.Parent
		if not parent then
			error("cannot encode an unparented clipboard route")
		end
		local ordinal = 0
		for _, sibling in ipairs(parent:GetChildren()) do
			if sibling.Name == current.Name and sibling.ClassName == current.ClassName then
				ordinal += 1
				if sibling == current then
					break
				end
			end
		end
		if ordinal < 1 then
			error("could not encode clipboard parent route")
		end
		table.insert(route, 1, {
			name = current.Name,
			class = current.ClassName,
			ordinal = ordinal,
		})
		current = parent
	end
	return route
end

local function resolveRoute(route: any): Instance?
	if type(route) ~= "table" or #route < 1 or #route > 256 then
		return nil
	end
	local current: Instance = game
	for _, segment in ipairs(route) do
		if type(segment) ~= "table" then
			return nil
		end
		local name = tostring(segment.name or "")
		local className = tostring(segment.class or "")
		local wantedOrdinal = math.floor(tonumber(segment.ordinal) or 0)
		if name == "" or className == "" or wantedOrdinal < 1 then
			return nil
		end
		local ordinal = 0
		local matched = nil
		for _, child in ipairs(current:GetChildren()) do
			if child.Name == name and child.ClassName == className then
				ordinal += 1
				if ordinal == wantedOrdinal then
					matched = child
					break
				end
			end
		end
		if not matched then
			return nil
		end
		current = matched
	end
	return current
end

function Clipboard.install(deps: any)
	local serializationService: any = nil
	local serviceOk, serviceOrError = pcall(function()
		return game:GetService("SerializationService")
	end)
	if serviceOk then
		serializationService = serviceOrError
	end
	local methodsOk, methodsAvailable = pcall(function()
		return serializationService ~= nil
			and (serializationService :: any).SerializeInstancesAsync ~= nil
			and (serializationService :: any).DeserializeInstancesAsync ~= nil
	end)
	local available = serializationService ~= nil and methodsOk and methodsAvailable == true

	local function requireAvailable()
		if not available or serializationService == nil then
			error("Roblox SerializationService is unavailable in this Studio build")
		end
	end

	local function serializeRoots(roots: { Instance }, deadline: number): buffer
		requireAvailable()
		checkDeadline(deadline, "serialization")
		-- Studio's own Copy/Paste ignores Archivable=false. Temporarily matching
		-- that behavior is safe because every original value is restored even if
		-- the yielding serializer throws.
		local changed = {}
		for _, root in ipairs(roots) do
			setArchivableTree(root, true, changed)
		end
		local ok, first = pcall(function()
			return (serializationService :: any):SerializeInstancesAsync(roots)
		end)
		restoreArchivable(changed)
		if not ok or typeof(first) ~= "buffer" then
			error("native Studio serialization failed: " .. tostring(first))
		end
		checkDeadline(deadline, "serialization")

		-- SerializationService reads engine Source, while an open Script Editor can
		-- own newer text. Rehydrate detached roots, patch them from
		-- GetEditorSource(), and serialize once more so copied scripts match what
		-- the user can actually see in Studio.
		local detached = nil
		local decodedOk, decodedOrError = pcall(function()
			return (serializationService :: any):DeserializeInstancesAsync(first)
		end)
		if decodedOk and type(decodedOrError) == "table" then
			detached = decodedOrError
		end
		if detached == nil or #detached ~= #roots then
			if detached then
				destroyRoots(detached)
			end
			error("native Studio serialization could not be validated: " .. tostring(decodedOrError))
		end
		checkDeadline(deadline, "editor-source refresh")
		local patchOk, patchError = pcall(function()
			for index, original in ipairs(roots) do
				checkDeadline(deadline, "editor-source refresh")
				local copy = detached[index]
				setArchivableTree(copy, true, nil)
				patchVisibleScriptSources(original, copy, deps.readScriptSource)
			end
		end)
		if not patchOk then
			destroyRoots(detached)
			error(patchError)
		end
		checkDeadline(deadline, "final serialization")
		local finalOk, finalOrError = pcall(function()
			return (serializationService :: any):SerializeInstancesAsync(detached)
		end)
		destroyRoots(detached)
		if not finalOk or typeof(finalOrError) ~= "buffer" then
			error("native Studio serialization failed after editor-source refresh: " .. tostring(finalOrError))
		end
		checkDeadline(deadline, "final serialization")
		return finalOrError
	end

	local function upload(bytes: buffer, lease: any, deadline: number)
		if type(lease) ~= "table" then
			error("clipboard copy requires an artifact lease")
		end
		local artifactId = tostring(lease.id or "")
		local token = tostring(lease.token or "")
		if artifactId == "" or token == "" then
			error("clipboard artifact lease is incomplete")
		end
		local total = buffer.len(bytes)
		if total < 1 or total > MAX_BYTES or total > math.floor(tonumber(lease.maxBytes) or MAX_BYTES) then
			error(string.format("clipboard payload is outside the %d-byte limit", MAX_BYTES))
		end
		local offset = 0
		while offset < total do
			checkDeadline(deadline, "export")
			local count = math.min(CHUNK_BYTES, total - offset)
			local chunk = buffer.fromstring(buffer.readstring(bytes, offset, count))
			local ok, response = deps.httpJson("/artifacts/" .. artifactId .. "/chunk", "POST", {
				token = token,
				offset = offset,
				bytesBase64 = buffer.tostring(EncodingService:Base64Encode(chunk)),
			})
			if not ok or type(response) ~= "table" or response.ok ~= true then
				error("clipboard artifact upload failed: " .. errorDetail(response))
			end
			offset += count
		end
		checkDeadline(deadline, "artifact finalization")
		local ok, response = deps.httpJson("/artifacts/" .. artifactId .. "/finalize", "POST", {
			token = token,
			expectedSha256 = sha256BufferHex(bytes),
		})
		if not ok or type(response) ~= "table" or response.ok ~= true then
			error("clipboard artifact finalization failed: " .. errorDetail(response))
		end
		return response.artifact
	end

	local function download(args: any, deadline: number): buffer
		local artifactId = tostring(args.artifactId or "")
		local total = math.floor(tonumber(args.byteLength) or 0)
		local expectedSha = tostring(args.sha256 or ""):lower()
		if artifactId == "" or total < 1 or total > MAX_BYTES then
			error("clipboard paste received invalid artifact metadata")
		end
		if #expectedSha ~= 64 or expectedSha:find("[^0-9a-f]") then
			error("clipboard paste received an invalid SHA-256 digest")
		end
		local output = buffer.create(total)
		local offset = 0
		while offset < total do
			checkDeadline(deadline, "download")
			local ok, response = deps.httpJson("/artifacts/" .. artifactId .. "/read", "POST", {
				offset = offset,
				maxBytes = math.min(CHUNK_BYTES, total - offset),
			})
			if not ok or type(response) ~= "table" or response.ok ~= true or type(response.chunk) ~= "table" then
				error("clipboard artifact download failed: " .. errorDetail(response))
			end
			local chunk = response.chunk
			local chunkOffset = math.floor(tonumber(chunk.offset) or -1)
			local nextOffset = math.floor(tonumber(chunk.nextOffset) or -1)
			if chunkOffset ~= offset or nextOffset <= offset or nextOffset > total then
				error("clipboard artifact returned an invalid chunk range")
			end
			if
				math.floor(tonumber(chunk.byteLength) or -1) ~= total
				or tostring(chunk.sha256 or ""):lower() ~= expectedSha
			then
				error("clipboard artifact metadata changed during download")
			end
			local decoded = EncodingService:Base64Decode(buffer.fromstring(tostring(chunk.bytesBase64 or "")))
			if buffer.len(decoded) ~= nextOffset - offset then
				error("clipboard artifact returned an invalid decoded chunk length")
			end
			buffer.copy(output, offset, decoded, 0, buffer.len(decoded))
			offset = nextOffset
		end
		if sha256BufferHex(output) ~= expectedSha then
			error("clipboard artifact failed SHA-256 verification")
		end
		checkDeadline(deadline, "download")
		return output
	end

	local handlers = {}

	handlers.clipboard_copy = function(args: any)
		requireAvailable()
		local deadline = clipboardDeadline(args.timeoutSeconds, args.__rosyncReceivedAt)
		local roots = normalizeRoots(args.paths, deps.resolvePath)
		local metadata = {}
		for _, root in ipairs(roots) do
			local parent = root.Parent
			if parent == nil then
				error("copy root lost its parent during serialization")
			end
			table.insert(metadata, {
				sourcePath = deps.pathOf(root),
				parentPath = deps.pathOf(parent),
				parentRoute = routeForInstance(parent),
				class = root.ClassName,
				name = root.Name,
			})
		end
		local bytes = serializeRoots(roots, deadline)
		local artifact = upload(bytes, args.lease, deadline)
		return {
			artifact = artifact,
			byteLength = buffer.len(bytes),
			sha256 = sha256BufferHex(bytes),
			roots = metadata,
			gameId = tostring(game.GameId),
			placeId = tostring(game.PlaceId),
		}
	end

	handlers.clipboard_paste = function(args: any)
		requireAvailable()
		local deadline = clipboardDeadline(args.timeoutSeconds, args.__rosyncReceivedAt)
		local metadata = args.roots
		if type(metadata) ~= "table" or #metadata < 1 or #metadata > MAX_ROOTS then
			error("clipboard paste received invalid root metadata")
		end
		local bytes = download(args, deadline)
		checkDeadline(deadline, "deserialization")
		local ok, instancesOrError = pcall(function()
			return (serializationService :: any):DeserializeInstancesAsync(bytes)
		end)
		if not ok or type(instancesOrError) ~= "table" then
			error("native Studio deserialization failed: " .. tostring(instancesOrError))
		end
		checkDeadline(deadline, "deserialization")
		local instances = instancesOrError :: { Instance }
		if #instances ~= #metadata then
			destroyRoots(instances)
			error(string.format("clipboard decoded %d roots; expected %d", #instances, #metadata))
		end
		local explicitParent = tostring(args.parent or "")
		local parents = {}
		for index, rootMeta in ipairs(metadata) do
			checkDeadline(deadline, "destination resolution")
			if type(rootMeta) ~= "table" then
				destroyRoots(instances)
				error("clipboard root metadata is malformed")
			end
			local parentPath = if explicitParent ~= "" then explicitParent else tostring(rootMeta.parentPath or "")
			local parent = if explicitParent ~= ""
				then deps.resolvePath(parentPath)
				else resolveRoute(rootMeta.parentRoute)
			if not parent then
				destroyRoots(instances)
				error("paste parent not found: " .. parentPath .. " (pass --to to choose a destination)")
			end
			parents[index] = parent
		end

		local label = string.format("Ro Sync paste (%d instance%s)", #instances, if #instances == 1 then "" else "s")
		checkDeadline(deadline, "change-history setup")
		local recordingId = ChangeHistoryService:TryBeginRecording(label, label)
		if recordingId == nil then
			destroyRoots(instances)
			error("Studio refused to begin paste change-history recording")
		end
		local applied, applyError = pcall(function()
			for index, inst in ipairs(instances) do
				checkDeadline(deadline, "paste")
				inst.Archivable = true
				inst.Parent = parents[index]
			end
			if args.select ~= false then
				checkDeadline(deadline, "selection")
				Selection:Set(instances)
			end
			checkDeadline(deadline, "commit")
		end)
		if not applied then
			destroyRoots(instances)
			pcall(function()
				ChangeHistoryService:FinishRecording(recordingId, Enum.FinishRecordingOperation.Cancel)
			end)
			error("paste was rolled back: " .. tostring(applyError))
		end
		local committed, commitError = pcall(function()
			ChangeHistoryService:FinishRecording(recordingId, Enum.FinishRecordingOperation.Commit)
		end)
		if not committed then
			destroyRoots(instances)
			pcall(function()
				ChangeHistoryService:FinishRecording(recordingId, Enum.FinishRecordingOperation.Cancel)
			end)
			error("paste commit failed and was rolled back: " .. tostring(commitError))
		end
		local paths = {}
		for _, inst in ipairs(instances) do
			table.insert(paths, deps.pathOf(inst))
		end
		return {
			count = #instances,
			paths = paths,
			selected = args.select ~= false,
			parent = if explicitParent ~= "" then explicitParent else nil,
		}
	end

	return {
		available = available,
		handlers = handlers,
	}
end

return Clipboard
