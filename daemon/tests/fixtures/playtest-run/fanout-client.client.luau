local payload = {
	context = playtest.context,
	marker = playtest.args.marker,
}
playtest.signal("fanout-ready", payload)
return payload
