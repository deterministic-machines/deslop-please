package code

// Code is an enum: its values are typed constants, named by its String.
type Code int

const (
	OK               Code = 0
	Cancelled        Code = 1
	Unknown          Code = 2
	InvalidArgument  Code = 3
	DeadlineExceeded Code = 4
	NotFound         Code = 5
	AlreadyExists    Code = 6
	PermissionDenied Code = 7
	Internal         Code = 8
)

func (c Code) String() string {
	switch c {
	case OK:
		return "ok"
	case Cancelled:
		return "cancelled"
	case Unknown:
		return "unknown"
	case InvalidArgument:
		return "invalid argument"
	case DeadlineExceeded:
		return "deadline exceeded"
	case NotFound:
		return "not found"
	case AlreadyExists:
		return "already exists"
	case PermissionDenied:
		return "permission denied"
	case Internal:
		return "internal"
	}
	return "code"
}
