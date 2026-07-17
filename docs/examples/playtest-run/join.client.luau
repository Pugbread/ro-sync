-- This path is intentionally game-specific: adapt it to the project's network
-- package and queue hierarchy while keeping the playscript coordination shape.
local Net = require(game:GetService("ReplicatedStorage"):WaitForChild("Packages"):WaitForChild("net"))
local queue = workspace:WaitForChild("Ques"):WaitForChild("1")

Net:RemoteFunction("JoinQueue"):InvokeServer(queue)
Net:RemoteFunction("VoteMap"):InvokeServer(playtest.args.map)
playtest.signal("voted")

return "ok"
