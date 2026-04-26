# Ro Sync value schema

Shared single source of truth for the type-tagged JSON shapes that flow over
`/push`, `/poll`, and the WebSocket channel. The **plugin** encodes
(`encodeValue` / `decodeValue` in `plugin/Plugin.luau`) and the **daemon**
must decode (and re-encode for reverse direction) with exactly the same shape.

> `.meta.json` is **not** a supported artifact in Ro Sync. Property/attribute
> persistence is intentionally out of scope — only scripts and folder-ish
> container shapes round-trip to disk. The encodings below are used for
> in-memory / wire traffic (e.g. `tree.json` skeleton values, `set`/`update`
> op payloads), not for on-disk meta files.

## General rules

- **Primitives pass through unwrapped.** `boolean`, finite `number`,
  `string` → emitted as JSON bool / number / string.
- **Non-finite numbers** (`NaN`, `±Inf`) are dropped by the plugin (JSON has
  no representation). Daemon should treat missing keys as default.
- **Everything else** is a JSON object with a `"__type"` discriminator field.
  Unknown `__type` values are passed through unchanged on the plugin side
  (forward-compat). The daemon SHOULD reject unknowns loudly instead.
- **`nil` / absent values are never emitted.** A missing key in the JSON
  object means "default" (the value equals what `Instance.new(ClassName)`
  would produce for that property).
- **Property names are PascalCase** exactly as Roblox exposes them
  (`BackgroundColor3`, not `backgroundColor3`). Attribute names are free-form
  but constrained by Roblox's own rules (see validator in `Plugin.luau`).

## Encoded type catalogue

### Geometry / math

| Roblox type      | JSON shape                                                                    |
|------------------|-------------------------------------------------------------------------------|
| `Vector2`        | `{"__type":"Vector2","x":N,"y":N}`                                            |
| `Vector3`        | `{"__type":"Vector3","x":N,"y":N,"z":N}`                                      |
| `Vector2int16`   | `{"__type":"Vector2int16","x":N,"y":N}`                                       |
| `Vector3int16`   | `{"__type":"Vector3int16","x":N,"y":N,"z":N}`                                 |
| `CFrame`         | `{"__type":"CFrame","components":[x,y,z,r00,r01,r02,r10,r11,r12,r20,r21,r22]}` |
| `UDim`           | `{"__type":"UDim","s":N,"o":N}` (scale, offset)                               |
| `UDim2`          | `{"__type":"UDim2","xs":N,"xo":N,"ys":N,"yo":N}`                              |
| `Rect`           | `{"__type":"Rect","minx":N,"miny":N,"maxx":N,"maxy":N}`                       |
| `NumberRange`    | `{"__type":"NumberRange","min":N,"max":N}`                                    |
| `Ray`            | `{"__type":"Ray","origin":[x,y,z],"direction":[x,y,z]}`                       |

CFrame component order is Roblox's `CFrame:GetComponents()` — position first,
then the rotation matrix in row-major order.

### Colour

| Roblox type    | JSON shape                                                    |
|----------------|---------------------------------------------------------------|
| `Color3`       | `{"__type":"Color3","r":N,"g":N,"b":N}` (all floats 0..1)     |
| `BrickColor`   | `{"__type":"BrickColor","number":N}` (BrickColor palette idx) |

### Sequences

| Roblox type       | JSON shape                                                                              |
|-------------------|-----------------------------------------------------------------------------------------|
| `NumberSequence`  | `{"__type":"NumberSequence","keypoints":[{"t":N,"value":N,"envelope":N}, ...]}`         |
| `ColorSequence`   | `{"__type":"ColorSequence","keypoints":[{"t":N,"r":N,"g":N,"b":N}, ...]}`               |

Keypoint `t` is in `[0, 1]`; sequences need at least 2 keypoints with the
first at `t=0` and last at `t=1` (Roblox constraint — daemon should not
re-order, just pass through).

### Enums

| Roblox type  | JSON shape                                                 |
|--------------|------------------------------------------------------------|
| `EnumItem`   | `{"__type":"Enum","enum":"Material","name":"Plastic"}`     |

`enum` is the Enum category short name (without `Enum.` prefix). `name` is
the member name. The decoder accepts `"__type":"EnumItem"` as a legacy alias.

### Typography

| Roblox type  | JSON shape                                                                       |
|--------------|----------------------------------------------------------------------------------|
| `Font`       | `{"__type":"Font","family":"rbxasset://...","weight":"Regular","style":"Normal"}` |

`family` is a content URI (usually `rbxasset://fonts/families/<Name>.json`).
`weight` and `style` are `Enum.FontWeight` / `Enum.FontStyle` member names.

### Physics

| Roblox type          | JSON shape                                                                                                         |
|----------------------|--------------------------------------------------------------------------------------------------------------------|
| `PhysicalProperties` | `{"__type":"PhysicalProperties","density":N,"friction":N,"elasticity":N,"frictionWeight":N,"elasticityWeight":N}`  |

When a BasePart's `CustomPhysicalProperties` is unset (material defaults),
the property is absent from the JSON entirely rather than encoded as `null`.

### Sets

| Roblox type  | JSON shape                                              |
|--------------|---------------------------------------------------------|
| `Axes`       | `{"__type":"Axes","axes":["X","Y"]}`                    |
| `Faces`      | `{"__type":"Faces","faces":["Top","Bottom","Front"]}`   |

### Instance references

```json
{"__type":"Instance","path":["Workspace","Baseplate"]}
```

Path is the full sequence from the DataModel root (first segment is a
service name: `Workspace`, `ReplicatedStorage`, `ServerScriptService`, ...).
An empty path or one that fails to resolve decodes to `nil`.

Used for `ObjectValue.Value`, `PrimaryPart`, `Attachment0` / `Attachment1`
on `Beam` / `Trail`, `SoundGroup`, etc.

## Properties covered by the walker

The plugin's walker iterates a curated `CANDIDATE_PROPS` list (≈150 entries)
and only emits values that differ from the per-class default obtained via
`Instance.new(ClassName)`. Classes currently hit include:

- Every `BasePart` subclass (`Part`, `MeshPart`, `WedgePart`,
  `UnionOperation`, `SpawnLocation`, `Seat`, `VehicleSeat`, ...)
- `Decal`, `Texture`, `SpecialMesh`
- `GuiObject` subclasses (`Frame`, `TextLabel`, `TextButton`, `TextBox`,
  `ImageLabel`, `ImageButton`, `ScrollingFrame`, `ViewportFrame`,
  `VideoFrame`, `CanvasGroup`)
- `UIStroke`, `UIGradient`, `UICorner`, `UIPadding`, `UIListLayout`,
  `UIGridLayout`, `UIScale`, `UIAspectRatioConstraint`,
  `UISizeConstraint`, `UITextSizeConstraint`
- `PointLight`, `SpotLight`, `SurfaceLight`
- `Fire`, `Smoke`, `Sparkles`, `ParticleEmitter`, `Trail`, `Beam`
- `Attachment`, `Camera`
- `Sound`, `SoundGroup`
- `ClickDetector`, `ProximityPrompt`
- `Tool`, `Model`, `Folder`
- All `ValueBase` subclasses (`StringValue`, `IntValue`, `NumberValue`,
  `BoolValue`, `CFrameValue`, `Vector3Value`, `Color3Value`,
  `BrickColorValue`, `ObjectValue`, `RayValue`)
- `LuaSourceContainer` (Source via `ScriptEditorService`)

Services themselves (`Workspace`, `Lighting`, ...) are NOT walked for
properties — `Instance.new(serviceName)` errors, so the template-comparison
machinery has no ground truth to compare against. They still round-trip
their children and attributes.

## Op schema (reminder)

The `/push` and `/poll` ops share this shape (plugin → daemon and daemon →
plugin):

- `{"op":"set","path":[...parent segments],"node":{class,name,properties,children}}`
- `{"op":"delete","path":[...segments of deleted instance]}`
- `{"op":"update","path":[...segments],"properties":{...}}` — plain property
  update; never carries a `name` field (renames are a separate op).
- `{"op":"rename","path":[...OLD segments],"name":"NewName"}` — path points
  to where the instance USED to live on disk; daemon renames that file/dir.
- `{"op":"move","from":[...old segments],"to":[...new parent segments]}`

Plugin emits `rename` using a cached "last known name" table (see
`lastKnownName` in `Plugin.luau`) because `Changed("Name")` fires after the
assignment and `pathFromRoot` would otherwise reconstruct the new path.
