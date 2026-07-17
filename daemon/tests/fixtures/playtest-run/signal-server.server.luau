local ready = playtest.awaitClients(1, 30)
if not ready then
	playtest.fail("client context did not become ready")
	return nil
end

local listening = playtest.await("fixture-client-listening", 10)
if listening == nil then
	playtest.fail("client signal listener did not become ready")
	return nil
end

playtest.signal("fixture-server-ping", { sentBy = playtest.context })
local reply = playtest.await("fixture-client-pong", 10)
if reply == nil then
	playtest.fail("client signal timed out")
	return nil
end

return { roundTrip = true, listening = listening, reply = reply }
