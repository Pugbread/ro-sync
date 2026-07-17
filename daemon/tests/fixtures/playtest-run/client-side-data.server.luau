local clients = playtest.awaitClients(playtest.args.clients or 1, 30)
if not clients then
	playtest.fail("required client contexts did not become ready")
	return nil
end

-- Give every companion enough time to return or fail. Client outcomes are
-- side data; this main result deliberately succeeds either way.
task.wait(2)
return { mainSucceeded = true, clients = clients }
