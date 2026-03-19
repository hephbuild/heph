package hkv

func StringCodec() Codec[string] {
	return &stringCodec{}
}

type stringCodec struct{}

func (p *stringCodec) Marshal(value string) ([]byte, error) {
	return []byte(value), nil
}

func (p *stringCodec) Unmarshal(data []byte) (string, error) {
	return string(data), nil
}
