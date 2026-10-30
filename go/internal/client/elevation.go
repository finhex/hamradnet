package client

// IsElevated reports whether this process can create a network interface.
//
// On Unix that means being root. On Windows there is no clean way to ask without
// pulling in the Windows API packages, so it probes a location only
// administrators can write to.
func IsElevated() bool {
	return isElevated()
}
