playtest.signal("fixture-client-ready", {
	context = playtest.context,
	jobId = playtest.jobId,
})

return { joined = true, context = playtest.context }
