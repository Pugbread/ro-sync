playtest.emit({ phase = "fixture-started", args = playtest.args })

return {
	ok = true,
	mode = playtest.mode,
	context = playtest.context,
	jobId = playtest.jobId,
	args = playtest.args,
}
