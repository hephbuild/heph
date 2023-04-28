package liblog

type Core interface {
	Enabled(Level) bool
	Log(Entry) error
}

type Collector interface {
	Write(Entry) error
}

func NewCore(collector Collector) Core {
	return core{
		collector: collector,
	}
}

type core struct {
	collector Collector
}

func (c core) Enabled(level Level) bool {
	return true
}

func (c core) Log(entry Entry) error {
	return c.collector.Write(entry)
}
