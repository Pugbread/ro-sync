task.spawn(function()
	task.wait(0.1)
	playtest.done({ completedBy = "spawn", context = playtest.context })
end)

return playtest.await("fixture-never-signalled", 30)
