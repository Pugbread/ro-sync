--!nonstrict

-- DataModel path and JSON-safe value codec for Ro Sync's remote-control
-- protocol. Keeping this recursion-heavy code in a packaged child module
-- reduces the size and long-lived register pressure of Plugin.luau.

local RemoteCodec = {}

local ENCODE_MAX_DEPTH = 64
local ENCODE_MAX_NODES = 20000
local ENCODE_MAX_TABLE_ENTRIES = 10000
local ENCODE_MAX_STRING_BYTES = 4 * 1024 * 1024

function RemoteCodec.install(deps)
	assert(type(deps) == "table", "RemoteCodec.install requires dependencies")
	assert(type(deps.resolvePath) == "function", "RemoteCodec.install requires resolvePath")

	local function splitPath(path: string): { string }
		local segments = {}
		if path == nil or path == "" or path == "/" then
			return segments
		end
		for segment in string.gmatch(tostring(path), "[^/]+") do
			table.insert(segments, segment)
		end
		return segments
	end

	local function resolvePath(path: string): Instance?
		local segments = splitPath(path)
		if #segments == 0 then
			return game
		end
		return deps.resolvePath(segments)
	end

	local function pathOf(inst: Instance): string
		if inst == nil or inst == game then
			return ""
		end
		local parts = {}
		local current = inst
		while current and current ~= game do
			table.insert(parts, 1, current.Name)
			current = current.Parent
		end
		return table.concat(parts, "/")
	end

	local encode
	function encode(value, depth, state)
		depth = depth or 0
		state = state or { nodes = 0, stringBytes = 0, seen = {} }
		if depth > ENCODE_MAX_DEPTH then
			error("remote value exceeds maximum nesting depth")
		end
		state.nodes += 1
		if state.nodes > ENCODE_MAX_NODES then
			error("remote value exceeds maximum node count")
		end
		local kind = typeof(value)
		if kind == "string" then
			state.stringBytes += #value
			if state.stringBytes > ENCODE_MAX_STRING_BYTES then
				error("remote value exceeds maximum string byte count")
			end
			return value
		elseif kind == "nil" or kind == "boolean" then
			return value
		elseif kind == "number" then
			if value ~= value or value == math.huge or value == -math.huge then
				return nil
			end
			return value
		elseif kind == "Vector2" then
			return { __type = "Vector2", x = value.X, y = value.Y }
		elseif kind == "Vector3" then
			return { __type = "Vector3", x = value.X, y = value.Y, z = value.Z }
		elseif kind == "CFrame" then
			return { __type = "CFrame", components = { value:GetComponents() } }
		elseif kind == "Color3" then
			return { __type = "Color3", r = value.R, g = value.G, b = value.B }
		elseif kind == "UDim" then
			return { __type = "UDim", scale = value.Scale, offset = value.Offset }
		elseif kind == "UDim2" then
			return {
				__type = "UDim2",
				x = { scale = value.X.Scale, offset = value.X.Offset },
				y = { scale = value.Y.Scale, offset = value.Y.Offset },
			}
		elseif kind == "EnumItem" then
			return {
				__type = "EnumItem",
				enum = tostring(value.EnumType),
				name = value.Name,
				value = value.Value,
			}
		elseif kind == "BrickColor" then
			return { __type = "BrickColor", name = value.Name }
		elseif kind == "NumberRange" then
			return { __type = "NumberRange", min = value.Min, max = value.Max }
		elseif kind == "Instance" then
			return { __type = "Instance", path = pathOf(value), class = value.ClassName }
		elseif kind == "table" then
			if state.seen[value] then
				error("remote value contains a table cycle")
			end
			state.seen[value] = true
			local isArray = true
			local entryCount = 0
			for key in pairs(value) do
				entryCount += 1
				if entryCount > ENCODE_MAX_TABLE_ENTRIES then
					state.seen[value] = nil
					error("remote table exceeds maximum entry count")
				end
				if type(key) ~= "number" then
					isArray = false
					break
				end
			end
			if isArray then
				local out = {}
				for index, item in ipairs(value) do
					out[index] = encode(item, depth + 1, state)
				end
				state.seen[value] = nil
				return out
			else
				local out = {}
				for key, item in pairs(value) do
					if type(key) == "string" then
						out[key] = encode(item, depth + 1, state)
					end
				end
				state.seen[value] = nil
				return out
			end
		end
		return tostring(value)
	end

	local function decode(value)
		local kind = type(value)
		if kind == "nil" or kind == "boolean" or kind == "string" or kind == "number" then
			return value
		end
		if kind ~= "table" then
			return value
		end
		local tag = value.__type
		if tag == nil then
			return value
		end
		if tag == "Vector2" then
			return Vector2.new(tonumber(value.x) or 0, tonumber(value.y) or 0)
		elseif tag == "Vector3" then
			return Vector3.new(tonumber(value.x) or 0, tonumber(value.y) or 0, tonumber(value.z) or 0)
		elseif tag == "CFrame" then
			local components = value.components or {}
			if #components == 3 then
				return CFrame.new(components[1], components[2], components[3])
			end
			if #components == 12 then
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
			error("CFrame components must have length 3 or 12 (got " .. #components .. ")")
		elseif tag == "Color3" then
			return Color3.new(tonumber(value.r) or 0, tonumber(value.g) or 0, tonumber(value.b) or 0)
		elseif tag == "UDim" then
			return UDim.new(tonumber(value.scale) or 0, tonumber(value.offset) or 0)
		elseif tag == "UDim2" then
			local x = value.x or {}
			local y = value.y or {}
			return UDim2.new(
				tonumber(x.scale) or 0,
				tonumber(x.offset) or 0,
				tonumber(y.scale) or 0,
				tonumber(y.offset) or 0
			)
		elseif tag == "EnumItem" then
			local enumName = tostring(value.enum or "")
			local enumType = Enum[enumName]
			if not enumType then
				error("unknown Enum: " .. enumName)
			end
			local ok, item = pcall(function()
				return enumType[tostring(value.name or "")]
			end)
			if not ok or item == nil then
				error("unknown EnumItem: " .. enumName .. "." .. tostring(value.name))
			end
			return item
		elseif tag == "BrickColor" then
			return BrickColor.new(tostring(value.name or "Medium stone grey") :: any)
		elseif tag == "NumberRange" then
			return NumberRange.new(tonumber(value.min) or 0, tonumber(value.max) or 0)
		elseif tag == "Instance" then
			local inst = resolvePath(tostring(value.path or ""))
			if not inst then
				error("instance not found: " .. tostring(value.path))
			end
			return inst
		end
		return value
	end

	return {
		splitPath = splitPath,
		resolvePath = resolvePath,
		pathOf = pathOf,
		encode = encode,
		decode = decode,
	}
end

return RemoteCodec
