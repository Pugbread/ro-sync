local attempted = 10_000

for sequence = 1, attempted do
	playtest.emit({ sequence = sequence })
end

return { attempted = attempted }
