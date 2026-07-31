--!nonstrict

-- Class, enum, and attribute introspection handlers for Ro Sync. The baked
-- fallback metadata is intentionally isolated from Plugin.luau so Studio
-- compiles it as a separate ModuleScript.

local Reflection = {}

function Reflection.install(deps)
	assert(type(deps) == "table", "Reflection.install requires dependencies")
	assert(type(deps.resolvePath) == "function", "Reflection.install requires resolvePath")
	assert(type(deps.pathOf) == "function", "Reflection.install requires pathOf")
	assert(type(deps.decode) == "function", "Reflection.install requires decode")
	assert(type(deps.findLimitDefault) == "number", "Reflection.install requires findLimitDefault")

	local handlers = {}

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

	handlers.class_info = function(args)
		local className = tostring(args.class_name or args.className or "")
		if className == "" then
			error("missing class_name")
		end
		-- ReflectionService is the complete, documented reflection surface, and it
		-- is strictly better than both paths below: for BasePart it reports 90
		-- properties, 76 methods and 16 events, where the StudioService path
		-- returns no methods at all and the baked table carries roughly 20
		-- properties and six hand-listed method names. Try it first; the older
		-- paths remain for Studio builds that do not expose it.
		local reflectOk, reflected = pcall(function()
			local reflection = game:GetService("ReflectionService")

			local function typeName(value): string
				if type(value) ~= "table" then
					return tostring(value or "")
				end
				return tostring(value.ScriptType or value.EngineType or "")
			end

			local function signature(entry, returns: boolean): string
				local parts = {}
				if type(entry.Parameters) == "table" then
					for _, parameter in ipairs(entry.Parameters) do
						local rendered =
							string.format("%s: %s", tostring(parameter.Name or "?"), typeName(parameter.Type))
						if parameter.DefaultValue ~= nil then
							rendered ..= " = " .. tostring(parameter.DefaultValue)
						end
						table.insert(parts, rendered)
					end
				end
				local rendered = string.format("%s(%s)", tostring(entry.Name or ""), table.concat(parts, ", "))
				if returns then
					local returnType = typeName(entry.ReturnType)
					if returnType ~= "" then
						rendered ..= " -> " .. returnType
					end
				end
				return rendered
			end

			local properties = {}
			for _, entry in ipairs(reflection:GetPropertiesOfClass(className)) do
				local display = entry.Display
				table.insert(properties, {
					name = tostring(entry.Name or ""),
					category = if type(display) == "table" then tostring(display.Category or "") else "",
					type = typeName(entry.Type),
				})
			end

			local methods = {}
			for _, entry in ipairs(reflection:GetMethodsOfClass(className)) do
				table.insert(methods, signature(entry, true))
			end

			local events = {}
			for _, entry in ipairs(reflection:GetEventsOfClass(className)) do
				table.insert(events, signature(entry, false))
			end

			if #properties == 0 and #methods == 0 and #events == 0 then
				return nil
			end

			table.sort(properties, function(a, b)
				return a.name < b.name
			end)
			table.sort(methods)
			table.sort(events)

			return { properties = properties, methods = methods, events = events }
		end)
		if reflectOk and type(reflected) == "table" then
			return reflected
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

	handlers.enum_list = function(args)
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

	handlers.enums = function(_args)
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

	handlers.find_by_attr = function(args)
		local attrName = tostring(args.name or "")
		if attrName == "" then
			error("missing name")
		end
		local rootPath = args.under or args.root or ""
		local root = deps.resolvePath(tostring(rootPath))
		if not root then
			error("root not found: " .. tostring(rootPath))
		end
		local limit = tonumber(args.limit) or deps.findLimitDefault
		local hasValue = args.value ~= nil
		local target = nil
		if hasValue then
			local dok, decoded = pcall(deps.decode, args.value)
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
						table.insert(out, deps.pathOf(c))
					end
				end
				walk(c)
			end
		end
		walk(root)
		return out
	end

	return {
		handlers = handlers,
	}
end

return Reflection
