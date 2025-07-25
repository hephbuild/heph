package main

import (
	"flag"
	"fmt"
	"google.golang.org/protobuf/compiler/protogen"
	"google.golang.org/protobuf/reflect/protoreflect"
	"google.golang.org/protobuf/types/descriptorpb"
	"google.golang.org/protobuf/types/pluginpb"
	"path/filepath"
	"regexp"
)

var (
	flags flag.FlagSet
)

func main() {
	protogen.Options{ParamFunc: flags.Set}.Run(Generate)
}

var SupportedFeatures = uint64(pluginpb.CodeGeneratorResponse_FEATURE_PROTO3_OPTIONAL | pluginpb.CodeGeneratorResponse_FEATURE_SUPPORTS_EDITIONS)

var SupportedEditionsMinimum = descriptorpb.Edition_EDITION_PROTO2
var SupportedEditionsMaximum = descriptorpb.Edition_EDITION_2023

type Helpers struct {
	descriptor map[protoreflect.Descriptor]string
	kind       map[protoreflect.Kind]string

	generators   []func(g *protogen.GeneratedFile)
	goPkgName    protogen.GoPackageName
	goImportPath protogen.GoImportPath
}

func (h *Helpers) WriterFnField(field *protogen.Field) string {
	switch {
	case field.Message != nil:
		return h.WriterFnMessage(field.Message)
	case field.Enum != nil:
		return h.WriterFnEnum(field.Enum)
	default:
		return h.WriterFnPrimitive(field.Desc)
	}
}

func (h *Helpers) WriterFnEnum(enum *protogen.Enum) string {
	desc := enum.Desc

	fn, ok := h.descriptor[desc]
	if !ok {
		fn = stablewriteFuncName(desc)

		h.generators = append(h.generators, func(g *protogen.GeneratedFile) {
			g.P("func ", fn, "(w ", ioWriter, ", v ", enum.GoIdent, ") error {")
			primitiveGenerator(protoreflect.EnumKind)(g)
			g.P("}")
		})

		h.descriptor[desc] = fn
	}

	return fn
}

var goTypeForKind = map[protoreflect.Kind]string{ //nolint:exhaustive
	protoreflect.BoolKind:     "bool",
	protoreflect.Int32Kind:    "int32",
	protoreflect.Sint32Kind:   "int32",
	protoreflect.Uint32Kind:   "uint32",
	protoreflect.Int64Kind:    "int64",
	protoreflect.Sint64Kind:   "int64",
	protoreflect.Uint64Kind:   "uint64",
	protoreflect.Sfixed32Kind: "int32",
	protoreflect.Fixed32Kind:  "int32",
	protoreflect.FloatKind:    "float32",
	protoreflect.Sfixed64Kind: "int64",
	protoreflect.Fixed64Kind:  "int64",
	protoreflect.DoubleKind:   "float64",
	protoreflect.StringKind:   "string",
	protoreflect.BytesKind:    "[]byte",
	protoreflect.EnumKind:     "int32",
}

func (h *Helpers) WriterFnPrimitive(desc protoreflect.FieldDescriptor) string {
	goType, ok := goTypeForKind[desc.Kind()]
	if !ok {
		panic(fmt.Sprintf("not supposed to happen: %v %v", desc.Kind(), goType))
	}

	fn, ok := h.kind[desc.Kind()]
	if !ok {
		fn = stablewriteFuncNameDesc(desc)

		h.generators = append(h.generators, func(g *protogen.GeneratedFile) {
			g.P("func ", fn, "(w ", ioWriter, ", v ", goType, ") error {")
			primitiveGenerator(desc.Kind())(g)
			g.P("}")
		})

		h.kind[desc.Kind()] = fn
	}

	return fn
}

func primitiveGenerator(kind protoreflect.Kind) func(g *protogen.GeneratedFile) {
	switch kind {
	case protoreflect.BoolKind:
		return func(g *protogen.GeneratedFile) {
			g.P("if v {")
			g.P("_, err := w.Write([]byte{1})")
			g.P("return err")
			g.P("} else {")
			g.P("_, err := w.Write([]byte{0})")
			g.P("return err")
			g.P("}")
		}
	case protoreflect.EnumKind, protoreflect.Int32Kind, protoreflect.Int64Kind, protoreflect.Uint64Kind:
		return func(g *protogen.GeneratedFile) {
			g.P("_, err := ", writeVarint, "(w, uint64(v))")
			g.P("return err")
		}
	case protoreflect.Sint32Kind:
	case protoreflect.Uint32Kind:
	case protoreflect.Sint64Kind:
	case protoreflect.Sfixed32Kind:
	case protoreflect.Fixed32Kind:
		return func(g *protogen.GeneratedFile) {
			g.P("_, err := w.Write(", appendFixed32Fn, "(nil, v))")
			g.P("return err")
		}
	case protoreflect.FloatKind:
		return func(g *protogen.GeneratedFile) {
			g.P("_, err := w.Write(", appendFixed32Fn, "(nil, ", float32BitsFn, "(v)))")
			g.P("return err")
		}
	case protoreflect.Sfixed64Kind:
	case protoreflect.Fixed64Kind:
		return func(g *protogen.GeneratedFile) {
			g.P("_, err := w.Write(", appendFixed64Fn, "(v))")
			g.P("return err")
		}

	case protoreflect.DoubleKind:
		return func(g *protogen.GeneratedFile) {
			g.P("_, err := w.Write(", appendFixed64Fn, "(nil, ", float64BitsFn, "(v)))")
			g.P("return err")
		}
	case protoreflect.StringKind:
		return func(g *protogen.GeneratedFile) {
			g.P("_, err := w.Write(", unsafeSliceFn, "(", unsafeStringDataFn, "(v),  len(v)))")
			g.P("return err")
		}
	case protoreflect.BytesKind:
		return func(g *protogen.GeneratedFile) {
			g.P("_, err := w.Write(v)")
			g.P("return err")
		}
	case protoreflect.MessageKind:
		panic("message: shouldnt happen")
	case protoreflect.GroupKind:
		panic("group: shouldnt happen")
	}

	return func(g *protogen.GeneratedFile) {
		g.P("panic(", fmt.Sprintf("TODO: %q", kind.String()), ")")
	}
}

func (h *Helpers) WriterFnMessage(msg *protogen.Message) string {
	fn, ok := h.descriptor[msg.Desc]
	if !ok {
		fn = stablewriteFuncName(msg.Desc)

		g := func(g *protogen.GeneratedFile) {
			g.P("func ", fn, "(w ", ioWriter, ", v *", msg.GoIdent, ") error {")
			for _, field := range msg.Fields {
				haser := fieldHaser("v", field)
				if haser != "" {
					g.P("if ", haser, "{")
				} else {
					g.P("{")
				}

				switch {
				case field.Desc.IsMap():
					g.P("v := ", fieldGetter("v", field))
					g.P("for _, k := range ", sortedFn, "(", mapKeysFn, "(v)) {")
					g.P("err := ", h.WriterFnPrimitive(field.Desc.MapKey()), "(w, k)")
					g.P("if err != nil { return err }")
					var childFn any
					if isPrimitive(field.Desc) {
						childFn = h.WriterFnPrimitive(field.Desc.MapValue())
					} else {
						childFn = stablewriteFuncNameDesc(field.Desc.MapValue())
					}
					g.P("err = ", childFn, "(w, v[k])")
					g.P("if err != nil { return err }")
					g.P("}")
				case field.Desc.IsList():
					childFn := h.WriterFnField(field)
					g.P("for _, v := range ", fieldGetter("v", field), "{")
					g.P("err := ", childFn, "(w, v)")
					g.P("if err != nil { return err }")
					g.P("}")
				default:
					childFn := h.WriterFnField(field)
					g.P("err := ", childFn, "(w, ", fieldGetter("v", field), ")")
					g.P("if err != nil { return err }")
				}

				g.P("}")
			}

			g.P("return nil")
			g.P("}")
		}

		h.generators = append(h.generators, g)

		h.descriptor[msg.Desc] = fn
	}

	return fn
}

func isPrimitive(desc protoreflect.FieldDescriptor) bool {
	switch desc.Kind() {
	case protoreflect.EnumKind:
		return false
	case protoreflect.MessageKind:
		return false
	case protoreflect.GroupKind:
		return false
	}

	return true
}

var nonIdentifierChars = regexp.MustCompile(`\W+`)

func stablewriteFuncName(md protoreflect.Descriptor) string {
	fqn := nonIdentifierChars.ReplaceAllLiteralString(string(md.FullName()), "_")
	return "stablewrite_" + fqn
}

func stablewriteFuncNameDesc(desc protoreflect.FieldDescriptor) string {
	switch desc.Kind() { //nolint:exhaustive
	case protoreflect.MessageKind, protoreflect.EnumKind:
		return stablewriteFuncName(desc)
	default:
		return "stablewrite_p_" + desc.Kind().String()
	}
}

func Generate(gen *protogen.Plugin) error {
	gen.SupportedEditionsMinimum = SupportedEditionsMinimum
	gen.SupportedEditionsMaximum = SupportedEditionsMaximum
	gen.SupportedFeatures = SupportedFeatures

	helpers := map[string]*Helpers{}

	for _, f := range gen.Files {
		if !f.Generate {
			continue
		}
		g := gen.NewGeneratedFile(f.GeneratedFilenamePrefix+"_stablewrite.pb.go", f.GoImportPath)
		g.P("// Code generated by protoc-gen-stablewrite. DO NOT EDIT")
		g.P()
		g.P("package ", f.GoPackageName)
		g.P()

		hkey := filepath.Join(filepath.Dir(f.GeneratedFilenamePrefix), "helpers_stablewrite.pb.go")
		h, ok := helpers[hkey]
		if !ok {
			h = &Helpers{
				descriptor:   map[protoreflect.Descriptor]string{},
				kind:         map[protoreflect.Kind]string{},
				goImportPath: f.GoImportPath,
				goPkgName:    f.GoPackageName,
			}
			helpers[hkey] = h
		}

		generateMessagesWriter(g, h, f.Messages)
		generateEnumWriter(g, h, f.Enums)
	}

	for k, h := range helpers {
		g := gen.NewGeneratedFile(k, h.goImportPath)
		g.P("// Code generated by protoc-gen-stablewrite. DO NOT EDIT")
		g.P()
		g.P("package ", h.goPkgName)
		g.P()
		generateHelpers(g, h)
	}

	return nil
}

func generateHelpers(g *protogen.GeneratedFile, h *Helpers) {
	for len(h.generators) > 0 {
		generators := h.generators
		h.generators = nil
		for _, gen := range generators {
			gen(g)
		}
	}
}

func generateEnumWriter(g *protogen.GeneratedFile, h *Helpers, enums []*protogen.Enum) {
	for _, enum := range enums {
		g.P("func (msg ", enum.GoIdent, ") StableWrite(w ", ioWriter, ") error {")
		fn := h.WriterFnEnum(enum)
		g.P("return ", fn, "(w, msg)")
		g.P("}")
	}
}

const (
	ioImp          = protogen.GoImportPath("io")
	unsafeImp      = protogen.GoImportPath("unsafe")
	slicesImp      = protogen.GoImportPath("slices")
	mapsImp        = protogen.GoImportPath("maps")
	mathImp        = protogen.GoImportPath("math")
	stablewriteImp = protogen.GoImportPath("github.com/hephbuild/heph/internal/hproto/protoc-gen-go-stablewrite/lib")
	protowireImp   = protogen.GoImportPath("google.golang.org/protobuf/encoding/protowire")
)

var (
	ioWriter = ioImp.Ident("Writer")

	unsafeSliceFn      = unsafeImp.Ident("Slice")
	unsafeStringDataFn = unsafeImp.Ident("StringData")
	sortedFn           = slicesImp.Ident("Sorted")
	mapKeysFn          = mapsImp.Ident("Keys")

	writeVarint = stablewriteImp.Ident("WriteVarint")

	appendFixed32Fn = protowireImp.Ident("AppendFixed32")
	appendFixed64Fn = protowireImp.Ident("AppendFixed64")

	float32BitsFn = mathImp.Ident("Float32bits")
	float64BitsFn = mathImp.Ident("Float64bits")
)

func generateMessagesWriter(g *protogen.GeneratedFile, h *Helpers, msgs []*protogen.Message) {
	for _, msg := range msgs {
		generateMessagesWriter(g, h, msg.Messages)
		generateEnumWriter(g, h, msg.Enums)

		if msg.Desc.IsMapEntry() {
			continue
		}

		g.P("func (msg *", msg.GoIdent, ") StableWrite(w ", ioWriter, ") error {")
		fn := h.WriterFnMessage(msg)
		g.P("return ", fn, "(w, msg)")
		g.P("}")
	}
}

func fieldHaser(v string, field *protogen.Field) string {
	if field.Desc.IsMap() || field.Desc.IsList() {
		return fieldGetter(v, field) + " != nil"
	}

	fn := methodName(field, "Has")
	if fn == "" {
		return ""
	}

	return v + "." + fn + "()"
}

func fieldGetter(v string, field *protogen.Field) string {
	return v + "." + methodName(field, "Get") + "()"
}

func methodName(field *protogen.Field, method string) string {
	name, _ := field.MethodName(method)

	return name
}
