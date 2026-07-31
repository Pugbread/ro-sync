--!nonstrict

-- Filesystem-safe naming, duplicate allocation, and generated Studio path
-- lookup for Ro Sync. Snapshot state is resolved lazily through the injected
-- getter because Plugin.luau initializes that mutually-dependent state later.

local PathHelpersModule = {}

function PathHelpersModule.install(deps)
	assert(type(deps) == "table", "PathHelpers.install requires dependencies")
	assert(type(deps.scriptClasses) == "table", "PathHelpers.install requires scriptClasses")
	assert(type(deps.getSnapshotApplyState) == "function", "PathHelpers.install requires getSnapshotApplyState")

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

	function PathHelpers.generatedPathSegmentForChild(
		child: Instance,
		taken: { [string]: true },
		nextOrdinals: { [string]: number }?
	): string
		local base = child.Name
		if not PathHelpers.pathSegmentTaken(taken, base) then
			taken[string.lower(base)] = true
			if nextOrdinals then
				nextOrdinals[string.lower(base)] = 1
			end
			return base
		end
		local baseKey = string.lower(base)
		local n = if nextOrdinals then nextOrdinals[baseKey] or 1 else 1
		while true do
			local candidate = string.format("%s [%d]", base, n)
			if not PathHelpers.pathSegmentTaken(taken, candidate) then
				taken[string.lower(candidate)] = true
				if nextOrdinals then
					nextOrdinals[baseKey] = n + 1
				end
				return candidate
			end
			n += 1
		end
	end

	PathHelpers.windowsReserved = {
		CON = true,
		PRN = true,
		AUX = true,
		NUL = true,
		COM1 = true,
		COM2 = true,
		COM3 = true,
		COM4 = true,
		COM5 = true,
		COM6 = true,
		COM7 = true,
		COM8 = true,
		COM9 = true,
		LPT1 = true,
		LPT2 = true,
		LPT3 = true,
		LPT4 = true,
		LPT5 = true,
		LPT6 = true,
		LPT7 = true,
		LPT8 = true,
		LPT9 = true,
		["CONIN$"] = true,
		["CONOUT$"] = true,
	}

	function PathHelpers.encodeName(name: string): string
		if name == "" then
			return "%_"
		end
		local out = {}
		for index = 1, #name do
			local byte = string.byte(name, index)
			local leadingDot = index == 1 and byte == 46
			local leadingSpace = index == 1 and byte == 32
			local trailingDotOrSpace = index == #name and (byte == 46 or byte == 32)
			local unsafe = byte >= 128
				or byte < 32
				or byte == 37
				or byte == 34
				or byte == 42
				or byte == 47
				or byte == 58
				or byte == 60
				or byte == 62
				or byte == 63
				or byte == 92
				or byte == 124
			if leadingDot or leadingSpace or trailingDotOrSpace or unsafe then
				out[#out + 1] = string.format("%%%02X", byte)
			else
				out[#out + 1] = string.char(byte)
			end
		end
		local encoded = table.concat(out)
		local reservedHead = string.upper(encoded:match("^([^.]+)") or encoded)
		if PathHelpers.windowsReserved[reservedHead] and #encoded > 0 then
			encoded = string.format("%%%02X", string.byte(encoded, 1)) .. encoded:sub(2)
		end
		-- A literal Roblox suffix in the generated ordinal grammar must not alias
		-- the allocator's own `<Name> [N]` lookup segment.
		local prefix, ordinal = encoded:match("^(.-) %[(%d+)%]$")
		if prefix and ordinal then
			encoded = prefix .. " %5B" .. ordinal .. "%5D"
		end
		return encoded
	end

	function PathHelpers.scriptSuffix(className: string): string
		if className == "Script" then
			return ".server.luau"
		elseif className == "LocalScript" then
			return ".client.luau"
		elseif className == "ModuleScript" then
			return ".luau"
		end
		return ""
	end

	function PathHelpers.isReservedInitStem(encoded: string): boolean
		local lower = string.lower(encoded)
		if lower == "init" then
			return true
		end
		return lower:match("^init %((.+)%)$") ~= nil
	end

	function PathHelpers.leafScriptStem(name: string, className: string): string
		local encoded = PathHelpers.encodeName(name)
		if deps.scriptClasses[className] and PathHelpers.isReservedInitStem(encoded) and #encoded > 0 then
			-- A literal leaf named `init` / `init (<Name>)` must not be parsed as
			-- the Source marker for its containing directory. Escape the first
			-- original byte while preserving the exact Roblox name on decode.
			return string.format("%%%02X", string.byte(encoded, 1)) .. encoded:sub(2)
		end
		return encoded
	end

	function PathHelpers.portableInitFileName(name: string, className: string): string
		local suffix = PathHelpers.scriptSuffix(className)
		local named = "init (" .. PathHelpers.encodeName(name) .. ")" .. suffix
		if #named <= 255 then
			return named
		end
		return "init" .. suffix
	end

	function PathHelpers.stripScriptSuffix(fragment: string): string
		local suffixes = {
			".server.luau",
			".client.luau",
			".server.lua",
			".client.lua",
			".luau",
			".lua",
		}
		for _, suffix in ipairs(suffixes) do
			if fragment:sub(-#suffix) == suffix then
				return fragment:sub(1, #fragment - #suffix)
			end
		end
		return fragment
	end

	function PathHelpers.lookupSegmentForPhysicalFragment(fragment: string, isDirectory: boolean): string
		return if isDirectory then fragment else PathHelpers.stripScriptSuffix(fragment)
	end

	function PathHelpers.isPortableDiskFragment(fragment): boolean
		return type(fragment) == "string"
			and fragment ~= ""
			and #fragment <= 32 * 1024
			and fragment ~= "."
			and fragment ~= ".."
			and not string.find(fragment, "\0", 1, true)
			and not string.find(fragment, "/", 1, true)
			and not string.find(fragment, "\\", 1, true)
	end

	function PathHelpers.sortKeyWithName(key: string?, name: string, mappedClass: string): string
		local signature = mappedClass
		if type(key) == "string" then
			local first = string.find(key, "\0", 1, true)
			local second = first and string.find(key, "\0", first + 1, true) or nil
			local third = second and string.find(key, "\0", second + 1, true) or nil
			if third then
				signature = string.sub(key, third + 1)
			end
		end
		return table.concat({
			string.lower(name),
			name,
			mappedClass,
			signature,
		}, "\0")
	end

	function PathHelpers.allocatePhysicalFragment(state, name: string, className: string, isDirectory: boolean): string
		local encoded = if not isDirectory and deps.scriptClasses[className]
			then PathHelpers.leafScriptStem(name, className)
			else PathHelpers.encodeName(name)
		local extension = if isDirectory then "" else PathHelpers.scriptSuffix(className)
		local base = encoded .. extension
		local baseKey = string.lower(base)
		if not state.taken[baseKey] then
			state.taken[baseKey] = true
			state.nextOrdinal[baseKey] = state.nextOrdinal[baseKey] or 1
			return base
		end
		local ordinal = state.nextOrdinal[baseKey] or 1
		while true do
			local candidate = string.format("%s [%d]%s", encoded, ordinal, extension)
			ordinal += 1
			if not state.taken[string.lower(candidate)] then
				state.taken[string.lower(candidate)] = true
				state.nextOrdinal[baseKey] = ordinal
				return candidate
			end
		end
	end

	function PathHelpers.mappedSyncClass(inst: Instance): string
		if inst:IsA("LuaSourceContainer") or inst.ClassName == "Folder" then
			return inst.ClassName
		end
		return "Folder"
	end

	function PathHelpers.findGeneratedPathChild(parent: Instance, seg: string, ctx): Instance?
		local state = deps.getSnapshotApplyState()
		if type(state) ~= "table" then
			error("snapshot apply state is not initialized", 2)
		end
		ctx = ctx or state.newContext({
			indexAvoidSyncCarriers = true,
		})
		local siblings = state.buildPhysicalSiblingIndex(parent, ctx)
		return siblings.byLookupSegment[seg]
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

	return PathHelpers
end

return PathHelpersModule
