package simpleregex

import (
	"regexp"
	"strings"
)

type Matcher struct {
	ms []patternMatcher
}

type patternMatcher struct {
	r     *regexp.Regexp
	names []string
}

func (m patternMatcher) Match(str string) (map[string]string, bool) {
	matches := m.r.FindStringSubmatch(str)
	if len(matches) == 0 {
		return map[string]string{}, false
	}

	res := map[string]string{}
	for i, match := range matches[1:] {
		res[m.names[i]] = match
	}

	return res, true
}

func parsePattern(pattern string) ([]string, string, error) {
	regexStr := "^"
	var names []string

	var inName bool
	var lastCommit int
	getSinceLastCommit := func(i int) string {
		if i < lastCommit {
			return ""
		}

		return pattern[lastCommit:i]
	}
	for i, c := range pattern {
		switch c {
		case '{':
			if !inName {
				inName = true
				regexStr += regexp.QuoteMeta(getSinceLastCommit(i))
				lastCommit = i + 1
				continue
			}
		case '}':
			if inName {
				inName = false
				name := getSinceLastCommit(i)
				if left, right, ok := strings.Cut(name, "!"); ok {
					name = left
					right = regexp.QuoteMeta(right)
					regexStr += "([^" + right + "]+)"
				} else {
					regexStr += "(.+)"
				}
				lastCommit = i + 1
				names = append(names, name)
				continue
			}
		}
	}
	regexStr += regexp.QuoteMeta(getSinceLastCommit(len(pattern) - 1))
	regexStr += "$"

	return names, regexStr, nil
}

func MustNew(patterns ...string) Matcher {
	m, err := New(patterns...)
	if err != nil {
		panic(err)
	}

	return m
}

func New(patterns ...string) (Matcher, error) {
	ms := make([]patternMatcher, 0, len(patterns))
	for _, pattern := range patterns {
		names, regexStr, err := parsePattern(pattern)
		if err != nil {
			return Matcher{}, err
		}

		r, err := regexp.Compile(regexStr)
		if err != nil {
			return Matcher{}, err
		}

		ms = append(ms, patternMatcher{
			r:     r,
			names: names,
		})
	}

	return Matcher{ms: ms}, nil
}

func (m Matcher) Match(str string) (map[string]string, bool) {
	for _, m := range m.ms {
		res, ok := m.Match(str)
		if ok {
			return res, true
		}
	}

	return nil, false
}

func (m Matcher) String() string {
	ss := make([]string, 0, len(m.ms))
	for _, m := range m.ms {
		ss = append(ss, m.r.String())
	}

	return strings.Join(ss, ", ")
}
