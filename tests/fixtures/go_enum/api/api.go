package api

import "example.com/enum/code"

func Status(found bool) string {
	if found {
		return code.OK.String()
	}
	return code.NotFound.String()
}
