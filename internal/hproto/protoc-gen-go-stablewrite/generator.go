// Copyright 2021-2022 Zenauth Ltd.
// SPDX-License-Identifier: Apache-2.0

package main

import (
	"fmt"
	"path/filepath"
	"regexp"
	"runtime/debug"
	"sort"

	"google.golang.org/protobuf/types/gofeaturespb"

	"google.golang.org/protobuf/compiler/protogen"
	"google.golang.org/protobuf/reflect/protoreflect"
	"google.golang.org/protobuf/types/descriptorpb"
	"google.golang.org/protobuf/types/pluginpb"
)

const (
	funcSuffix     = "_stablewrite"
	ioImp          = protogen.GoImportPath("io")
	mathImp        = protogen.GoImportPath("math")
	protowireImp   = protogen.GoImportPath("google.golang.org/protobuf/encoding/protowire")
	stablewriteImp = protogen.GoImportPath("github.com/hephbuild/heph/internal/hproto/protoc-gen-go-stablewrite/lib")
	mapsImp        = protogen.GoImportPath("maps")
	slicesImp      = protogen.GoImportPath("slices")
	unsafeImp      = protogen.GoImportPath("unsafe")
	receiverIdent  = "m"
	writerIdent    = "w"
)

var (
	Version = "dev"

	writerType = ioImp.Ident("Writer")

	appendFixed32Fn = protowireImp.Ident("AppendFixed32")
	appendFixed64Fn = protowireImp.Ident("AppendFixed64")
	encodeBoolFn    = protowireImp.Ident("EncodeBool")
	float32BitsFn   = mathImp.Ident("Float32bits")
	float64BitsFn   = mathImp.Ident("Float64bits")
	mapKeysFn       = mapsImp.Ident("Keys")
	sortedFn        = slicesImp.Ident("Sorted")

	unsafeSliceFn      = unsafeImp.Ident("Slice")
	unsafeStringDataFn = unsafeImp.Ident("StringData")

	writeVarint = stablewriteImp.Ident("WriteVarint")

	nonIdentifierChars = regexp.MustCompile(`\W+`)
)

func init() {
	if Version == "dev" {
		if bi, ok := debug.ReadBuildInfo(); ok {
			Version = bi.Main.Version
		}
	}
}

func Generate(p *protogen.Plugin) error {
	p.SupportedFeatures = uint64(pluginpb.CodeGeneratorResponse_FEATURE_PROTO3_OPTIONAL) | uint64(pluginpb.CodeGeneratorResponse_FEATURE_SUPPORTS_EDITIONS)
	p.SupportedEditionsMinimum = descriptorpb.Edition_EDITION_2023
	p.SupportedEditionsMaximum = descriptorpb.Edition_EDITION_2024
	// group files by import path because the helpers need to be generated at the package level.
	pkgFiles := make(map[protogen.GoImportPath][]*protogen.File)
	for _, f := range p.Files {
		if !f.Generate {
			continue
		}

		pkgFiles[f.GoImportPath] = append(pkgFiles[f.GoImportPath], f)
	}

	g := &codegen{Plugin: p}
	for _, files := range pkgFiles {
		g.generateHelpers(files)
		g.generateMethods(files)
	}

	return nil
}

type codegen struct {
	*protogen.Plugin
}

// generateHelpers generates helper functions for calculating the hash for each message type.
// Because messages can be recursive, we need to do this to avoid getting into an infinite loop.
func (g *codegen) generateHelpers(files []*protogen.File) {
	if len(files) == 0 {
		return
	}

	// find all messages referenced by the files.
	msgsToGen := make(map[string]*protogen.Message)
	for _, f := range files {
		for _, msg := range f.Messages {
			collectMessages(msgsToGen, msg)
		}
	}

	if len(msgsToGen) == 0 {
		return
	}

	fileName := filepath.Join(filepath.Dir(files[0].GeneratedFilenamePrefix), "stablewrite_helpers.pb.go")
	gf := g.newGeneratedFile(fileName, files, nil)

	// sort message names to make the generated file predictable (no spurious diffs)
	msgNames := make([]string, len(msgsToGen))
	i := 0
	for mn := range msgsToGen {
		msgNames[i] = mn
		i++
	}
	sort.Strings(msgNames)

	for _, mn := range msgNames {
		g.genHelperForMsg(gf, msgsToGen[mn])
		gf.P()
	}
}

func collectMessages(col map[string]*protogen.Message, msg *protogen.Message) {
	// ignore the special messages generated for map entries
	if msg.Desc.IsMapEntry() {
		for _, f := range msg.Fields {
			if f.Message != nil {
				collectMessages(col, f.Message)
			}
		}
		return
	}

	fnName := sumFuncName(msg.Desc)
	if _, ok := col[fnName]; ok {
		return
	}

	col[fnName] = msg
	for _, f := range msg.Fields {
		if f.Message != nil {
			collectMessages(col, f.Message)
		}
	}
}

func sumFuncName(md protoreflect.MessageDescriptor) string {
	fqn := nonIdentifierChars.ReplaceAllLiteralString(string(md.FullName()), "_")
	return fqn + funcSuffix
}

func (g *codegen) genHelperForMsg(gf *protogen.GeneratedFile, msg *protogen.Message) {
	fields := make([]*protogen.Field, len(msg.Fields))
	copy(fields, msg.Fields)

	sort.Slice(fields, func(i, j int) bool {
		return fields[i].Desc.Number() < fields[j].Desc.Number()
	})

	gf.P("// ", msg.Desc.FullName(), " from ", msg.Location.SourceFile)
	gf.P("func ", sumFuncName(msg.Desc), "(", receiverIdent, " *", msg.GoIdent, ",", writerIdent, " ", writerType, ", ignore map[string]struct{}) error {")

	oneOfs := make(map[string]struct{})

	for _, field := range fields {
		if field.Oneof != nil && !field.Oneof.Desc.IsSynthetic() {
			if _, ok := oneOfs[field.Oneof.GoName]; !ok {
				g.genOneOfField(gf, field)
				oneOfs[field.Oneof.GoName] = struct{}{}
			}
		} else {
			g.genField(gf, field)
		}
	}

	gf.P("return nil")
	gf.P("}")
}

func (g *codegen) genField(gf *protogen.GeneratedFile, field *protogen.Field) {
	gf.P("if _, ok := ignore[\"", field.Desc.FullName(), "\"]; !ok {")

	switch {
	case field.Desc.IsList():
		g.genListField(gf, field)
	case field.Desc.IsMap():
		g.genMapField(gf, field)
	default:
		gf.P("if ", fieldHaser(field), " {")
		g.genSingularField(gf, field.Desc, fieldAccess(field), false)
		gf.P("}")
	}

	gf.P("}")
}

func (g *codegen) genOneOfField(gf *protogen.GeneratedFile, field *protogen.Field) {
	if field.Parent.APILevel == gofeaturespb.GoFeatures_API_OPEN {
		g.genOneOfFieldOpen(gf, field)
	} else {
		g.genOneOfFieldOpaque(gf, field)
	}
}

func (g *codegen) genOneOfFieldOpen(gf *protogen.GeneratedFile, field *protogen.Field) {
	fieldName := oneOfAccess(field.Oneof)

	gf.P("if ", fieldName, " != nil {")
	gf.P("if _, ok := ignore[\"", field.Desc.ContainingOneof().FullName(), "\"]; !ok {")
	gf.P("switch t := ", fieldName, ".(type) {")
	for _, f := range field.Oneof.Fields {
		gf.P("case *", f.GoIdent, ":")
		g.genSingularField(gf, f.Desc, "t."+f.GoName, false)
	}
	gf.P("}")
	gf.P("}")
	gf.P("}")
}

func (g *codegen) genOneOfFieldOpaque(gf *protogen.GeneratedFile, field *protogen.Field) {
	fieldName := oneOfAccess(field.Oneof)

	gf.P("if ", fieldHaser(field), " {")
	gf.P("if _, ok := ignore[\"", field.Desc.ContainingOneof().FullName(), "\"]; !ok {")
	gf.P("switch ", fieldName, " {")
	for _, f := range field.Oneof.Fields {
		gf.P("case ", f.GoIdent, "_case:")
		g.genSingularField(gf, f.Desc, fieldAccess(f), false)
	}
	gf.P("}")
	gf.P("}")
	gf.P("}")
}

func (g *codegen) genListField(gf *protogen.GeneratedFile, field *protogen.Field) {
	fieldName := fieldAccess(field)
	gf.P("if ", fieldHaser(field), " {")
	gf.P("for _, v := range ", fieldName, " {")
	g.genSingularField(gf, field.Desc, "v", false)
	gf.P("}")
	gf.P("}")
}

func (g *codegen) genMapField(gf *protogen.GeneratedFile, field *protogen.Field) {
	fieldName := fieldAccess(field)
	gf.P("if ", fieldHaser(field), " {")
	gf.P("for _, k := range ", sortedFn, "(", mapKeysFn, "(", fieldName, ")) {")
	g.genSingularField(gf, field.Desc.MapKey(), "k", true)
	g.genSingularField(gf, field.Desc.MapValue(), fmt.Sprintf("%s[k]", fieldName), true)
	gf.P("}")
	gf.P("}")
}

func (g2 *codegen) genSingularField(g *protogen.GeneratedFile, fieldDesc protoreflect.FieldDescriptor, fieldName string, block bool) {
	if block {
		g.P("{")
		defer g.P("}")
	}

	switch fieldDesc.Kind() {
	case protoreflect.BoolKind:
		g.P("_, err := ", writeVarint, "(", writerIdent, ", ", encodeBoolFn, "(", fieldName, "))")
		g.P("if err != nil {return err}")
	case protoreflect.EnumKind, protoreflect.Int32Kind, protoreflect.Int64Kind, protoreflect.Uint64Kind:
		g.P("_, err := ", writeVarint, "(", writerIdent, ", uint64(", fieldName, "))")
		g.P("if err != nil {return err}")
	case protoreflect.Sint32Kind:
		g.P(`panic("TODO: `, fieldDesc.Kind().String(), `")`)
	case protoreflect.Uint32Kind:
		g.P(`panic("TODO: `, fieldDesc.Kind().String(), `")`)
	case protoreflect.Sint64Kind:
		g.P(`panic("TODO: `, fieldDesc.Kind().String(), `")`)
	case protoreflect.Sfixed32Kind:
		g.P(`panic("TODO: `, fieldDesc.Kind().String(), `")`)
	case protoreflect.Fixed32Kind:
		g.P("_, err := ", writerIdent, ".Write(", appendFixed32Fn, "(nil, ", fieldName, "))")
		g.P("if err != nil {return err}")
	case protoreflect.FloatKind:
		g.P("_, err := ", writerIdent, ".Write(", appendFixed32Fn, "(nil, ", float32BitsFn, "(", fieldName, ")))")
		g.P("if err != nil {return err}")
	case protoreflect.Sfixed64Kind:
		g.P(`panic("TODO: `, fieldDesc.Kind().String(), `")`)
	case protoreflect.Fixed64Kind:
		g.P("_, err := ", writerIdent, ".Write(", appendFixed64Fn, "(", fieldName, "))")
		g.P("if err != nil {return err}")
	case protoreflect.DoubleKind:
		g.P("_, err := ", writerIdent, ".Write(", appendFixed64Fn, "(nil, ", float64BitsFn, "(", fieldName, ")))")
		g.P("if err != nil {return err}")
	case protoreflect.StringKind:
		g.P("v := ", fieldName)
		g.P("_, err := ", writerIdent, ".Write(", unsafeSliceFn, "(", unsafeStringDataFn, "(v),  len(v)))")
		g.P("if err != nil {return err}")
	case protoreflect.BytesKind:
		g.P("_, err := ", writerIdent, ".Write(", fieldName, ")")
		g.P("if err != nil {return err}")
	case protoreflect.MessageKind:
		g.P("err := ", sumFuncName(fieldDesc.Message()), "(", fieldName, ",", writerIdent, ", ignore)")
		g.P("if err != nil {return err}")
	case protoreflect.GroupKind:
		panic("group: shouldnt happen")
	default:
		g.P(`panic("TODO: `, fieldDesc.Kind().String(), `")`)
	}
}

func fieldHaser(field *protogen.Field) string {
	if field.Desc.IsMap() || field.Desc.IsList() {
		return fieldAccess(field) + " != nil"
	}

	name, _ := field.MethodName("Has")
	if name == "" {
		return "true"
	}

	return fmt.Sprintf("%s.%s()", receiverIdent, name)
}

func fieldAccess(field *protogen.Field) string {
	name, _ := field.MethodName("Get")

	return fmt.Sprintf("%s.%s()", receiverIdent, name)
}

func oneOfAccess(field *protogen.Oneof) string {
	name := field.MethodName("Which")

	if name == "" {
		return fmt.Sprintf("%s.%s", receiverIdent, field.GoName)
	}

	return fmt.Sprintf("%s.%s()", receiverIdent, name)
}

// generateMethods generates helper methods (HashPB) for the top level messages defined in each file.
func (g *codegen) generateMethods(files []*protogen.File) {
	for _, f := range files {
		gf := g.newGeneratedFile(f.GeneratedFilenamePrefix+"_stablewrite.pb.go", files, f)
		genFuncs := make(map[string]struct{})

		for _, msg := range f.Messages {
			g.genMethodForMsg(gf, genFuncs, msg)
		}
	}
}

func (g *codegen) newGeneratedFile(filename string, files []*protogen.File, source *protogen.File) *protogen.GeneratedFile {
	gf := g.NewGeneratedFile(filename, files[0].GoImportPath)
	gf.P("// Code generated by protoc-gen-go-hashpb. DO NOT EDIT.")
	gf.P("// protoc-gen-go-stablewrite ", Version)
	if source != nil {
		gf.P("// Source: ", source.Desc.Path())
	}
	gf.P()
	gf.P("package ", files[0].GoPackageName)
	gf.P()
	return gf
}

func (g *codegen) genMethodForMsg(gf *protogen.GeneratedFile, genFuncs map[string]struct{}, msg *protogen.Message) {
	if msg.Desc.IsMapEntry() {
		return
	}

	if len(msg.Fields) == 0 {
		return
	}

	if _, ok := genFuncs[msg.GoIdent.GoName]; ok {
		return
	}

	genFuncs[msg.GoIdent.GoName] = struct{}{}

	gf.P("// StableWrite computes a hash of the message using the given hash function")
	gf.P("// The ignore set must contain fully-qualified field names (pkg.msg.field) that should be ignored from the hash")
	gf.P("func (", receiverIdent, " *", msg.GoIdent, ") StableWrite(", writerIdent, " ", writerType, ", ignore map[string]struct{}) error {")
	gf.P("if ", receiverIdent, " != nil {")
	gf.P("return ", sumFuncName(msg.Desc), "(", receiverIdent, ", ", writerIdent, ", ignore)")
	gf.P("}")
	gf.P("return nil")
	gf.P("}")
	gf.P()

	for _, msg := range msg.Messages {
		g.genMethodForMsg(gf, genFuncs, msg)
	}
}
