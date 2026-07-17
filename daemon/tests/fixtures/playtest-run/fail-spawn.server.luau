task.spawn(function()
	task.wait(0.1)
	playtest.fail("deliberate spawned fixture failure")
end)

return playtest.await("fixture-never-signalled", 30)
