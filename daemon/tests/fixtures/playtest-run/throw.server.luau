local function innerFailure()
	error("deliberate nested fixture failure")
end

local function outerFailure()
	innerFailure()
end

outerFailure()
return nil
