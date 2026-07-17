local ready = playtest.awaitClients(2, 30)
if not ready then
	playtest.fail("two client contexts did not become ready")
	return nil
end

local first = playtest.await("fanout-ready", 15)
local second = playtest.await("fanout-ready", 15)
if first == nil or second == nil then
	playtest.fail("both client companions did not signal readiness")
	return nil
end

-- Let both companion returns reach the edit-mode stream before the main
-- result owns teardown. This fixture verifies every-client fanout, not only
-- context creation.
task.wait(1)
return {
	clients = ready,
	signals = { first, second },
}
