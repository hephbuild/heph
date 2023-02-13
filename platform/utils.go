package platform

// HasAllLabels checks if candidate has all labels from required
func HasAllLabels(required, candidate map[string]string) bool {
	for k, v := range required {
		cv, ok := candidate[k]
		if !ok || cv != v {
			return false
		}
	}

	return true
}
