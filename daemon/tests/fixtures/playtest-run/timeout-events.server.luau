local sequence = 0

while true do
	sequence += 1
	playtest.emit({ phase = "waiting-for-timeout", sequence = sequence })
	task.wait(0.1)
end
