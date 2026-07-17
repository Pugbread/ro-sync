playtest.signal("fixture-client-listening", { sentBy = playtest.context })

local ping = playtest.await("fixture-server-ping", 10)
if ping == nil then
	error("server signal timed out")
end

local reply = {
	received = ping,
	sentBy = playtest.context,
}
playtest.signal("fixture-client-pong", reply)

return reply
