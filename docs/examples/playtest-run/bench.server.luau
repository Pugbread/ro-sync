-- This path is intentionally game-specific: adapt it to the project's match
-- service while keeping the playscript lifecycle and telemetry shape.
local MatchService = require(game:GetService("ServerScriptService").Server.MatchService)
local Players = game:GetService("Players")

playtest.awaitClients(1, 60)
playtest.await("voted", 30)

local player = Players:GetPlayers()[1]
if not player then
	playtest.fail("client became unavailable before the benchmark started")
	return nil
end

local match = MatchService:GetPlayerMatch(player)
repeat
	task.wait(0.25)
until match.State == "Racing" or match.IsDestroyed

playtest.emit({ phase = match.State })

local bots = {}
local startedAt = os.clock()

while match.State == "Racing" and not match.IsDestroyed do
	for name, ai in pairs(match.AICars) do
		local bot = bots[name] or { lap = 0, lapT = {} }
		bots[name] = bot

		local lap = ai.car:GetAttribute("Lap") or 0
		if lap > bot.lap then
			bot.lap = lap
			table.insert(bot.lapT, os.clock() - startedAt)
			playtest.emit({
				lap = lap,
				bot = name,
				t = os.clock() - startedAt,
			})
		end
	end

	local allDone = next(bots) ~= nil
	for _, bot in pairs(bots) do
		if bot.lap < playtest.args.laps then
			allDone = false
		end
	end
	if allDone then
		break
	end

	task.wait(0.3)
end

return bots
