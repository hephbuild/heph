// Code generated by github.com/Khan/genqlient, DO NOT EDIT.

package cloudclient

import (
	"context"
	"encoding/json"
	"fmt"
	"time"

	"github.com/Khan/genqlient/graphql"
)

// AuthActorAuth includes the requested fields of the GraphQL type Auth.
type AuthActorAuth struct {
	Actor AuthActorAuthActor `json:"-"`
}

// GetActor returns AuthActorAuth.Actor, and is useful for accessing the field via an interface.
func (v *AuthActorAuth) GetActor() AuthActorAuthActor { return v.Actor }

func (v *AuthActorAuth) UnmarshalJSON(b []byte) error {

	if string(b) == "null" {
		return nil
	}

	var firstPass struct {
		*AuthActorAuth
		Actor json.RawMessage `json:"actor"`
		graphql.NoUnmarshalJSON
	}
	firstPass.AuthActorAuth = v

	err := json.Unmarshal(b, &firstPass)
	if err != nil {
		return err
	}

	{
		dst := &v.Actor
		src := firstPass.Actor
		if len(src) != 0 && string(src) != "null" {
			err = __unmarshalAuthActorAuthActor(
				src, dst)
			if err != nil {
				return fmt.Errorf(
					"Unable to unmarshal AuthActorAuth.Actor: %w", err)
			}
		}
	}
	return nil
}

type __premarshalAuthActorAuth struct {
	Actor json.RawMessage `json:"actor"`
}

func (v *AuthActorAuth) MarshalJSON() ([]byte, error) {
	premarshaled, err := v.__premarshalJSON()
	if err != nil {
		return nil, err
	}
	return json.Marshal(premarshaled)
}

func (v *AuthActorAuth) __premarshalJSON() (*__premarshalAuthActorAuth, error) {
	var retval __premarshalAuthActorAuth

	{

		dst := &retval.Actor
		src := v.Actor
		var err error
		*dst, err = __marshalAuthActorAuthActor(
			&src)
		if err != nil {
			return nil, fmt.Errorf(
				"Unable to marshal AuthActorAuth.Actor: %w", err)
		}
	}
	return &retval, nil
}

// AuthActorAuthActor includes the requested fields of the GraphQL interface Actor.
//
// AuthActorAuthActor is implemented by the following types:
// AuthActorAuthActorOrganizationToken
// AuthActorAuthActorUser
// AuthActorAuthActorAnonymousActor
type AuthActorAuthActor interface {
	implementsGraphQLInterfaceAuthActorAuthActor()
	// GetTypename returns the receiver's concrete GraphQL type-name (see interface doc for possible values).
	GetTypename() string
	// GetActor_type returns the interface-field "actor_type" from its implementation.
	GetActor_type() string
	// GetActor_id returns the interface-field "actor_id" from its implementation.
	GetActor_id() string
}

func (v *AuthActorAuthActorOrganizationToken) implementsGraphQLInterfaceAuthActorAuthActor() {}
func (v *AuthActorAuthActorUser) implementsGraphQLInterfaceAuthActorAuthActor()              {}
func (v *AuthActorAuthActorAnonymousActor) implementsGraphQLInterfaceAuthActorAuthActor()    {}

func __unmarshalAuthActorAuthActor(b []byte, v *AuthActorAuthActor) error {
	if string(b) == "null" {
		return nil
	}

	var tn struct {
		TypeName string `json:"__typename"`
	}
	err := json.Unmarshal(b, &tn)
	if err != nil {
		return err
	}

	switch tn.TypeName {
	case "OrganizationToken":
		*v = new(AuthActorAuthActorOrganizationToken)
		return json.Unmarshal(b, *v)
	case "User":
		*v = new(AuthActorAuthActorUser)
		return json.Unmarshal(b, *v)
	case "AnonymousActor":
		*v = new(AuthActorAuthActorAnonymousActor)
		return json.Unmarshal(b, *v)
	case "":
		return fmt.Errorf(
			"response was missing Actor.__typename")
	default:
		return fmt.Errorf(
			`unexpected concrete type for AuthActorAuthActor: "%v"`, tn.TypeName)
	}
}

func __marshalAuthActorAuthActor(v *AuthActorAuthActor) ([]byte, error) {

	var typename string
	switch v := (*v).(type) {
	case *AuthActorAuthActorOrganizationToken:
		typename = "OrganizationToken"

		result := struct {
			TypeName string `json:"__typename"`
			*AuthActorAuthActorOrganizationToken
		}{typename, v}
		return json.Marshal(result)
	case *AuthActorAuthActorUser:
		typename = "User"

		result := struct {
			TypeName string `json:"__typename"`
			*AuthActorAuthActorUser
		}{typename, v}
		return json.Marshal(result)
	case *AuthActorAuthActorAnonymousActor:
		typename = "AnonymousActor"

		result := struct {
			TypeName string `json:"__typename"`
			*AuthActorAuthActorAnonymousActor
		}{typename, v}
		return json.Marshal(result)
	case nil:
		return []byte("null"), nil
	default:
		return nil, fmt.Errorf(
			`unexpected concrete type for AuthActorAuthActor: "%T"`, v)
	}
}

// AuthActorAuthActorAnonymousActor includes the requested fields of the GraphQL type AnonymousActor.
type AuthActorAuthActorAnonymousActor struct {
	Typename   string `json:"__typename"`
	Actor_type string `json:"actor_type"`
	Actor_id   string `json:"actor_id"`
}

// GetTypename returns AuthActorAuthActorAnonymousActor.Typename, and is useful for accessing the field via an interface.
func (v *AuthActorAuthActorAnonymousActor) GetTypename() string { return v.Typename }

// GetActor_type returns AuthActorAuthActorAnonymousActor.Actor_type, and is useful for accessing the field via an interface.
func (v *AuthActorAuthActorAnonymousActor) GetActor_type() string { return v.Actor_type }

// GetActor_id returns AuthActorAuthActorAnonymousActor.Actor_id, and is useful for accessing the field via an interface.
func (v *AuthActorAuthActorAnonymousActor) GetActor_id() string { return v.Actor_id }

// AuthActorAuthActorOrganizationToken includes the requested fields of the GraphQL type OrganizationToken.
type AuthActorAuthActorOrganizationToken struct {
	Typename   string `json:"__typename"`
	Actor_type string `json:"actor_type"`
	Actor_id   string `json:"actor_id"`
}

// GetTypename returns AuthActorAuthActorOrganizationToken.Typename, and is useful for accessing the field via an interface.
func (v *AuthActorAuthActorOrganizationToken) GetTypename() string { return v.Typename }

// GetActor_type returns AuthActorAuthActorOrganizationToken.Actor_type, and is useful for accessing the field via an interface.
func (v *AuthActorAuthActorOrganizationToken) GetActor_type() string { return v.Actor_type }

// GetActor_id returns AuthActorAuthActorOrganizationToken.Actor_id, and is useful for accessing the field via an interface.
func (v *AuthActorAuthActorOrganizationToken) GetActor_id() string { return v.Actor_id }

// AuthActorAuthActorUser includes the requested fields of the GraphQL type User.
type AuthActorAuthActorUser struct {
	Typename   string `json:"__typename"`
	Actor_type string `json:"actor_type"`
	Actor_id   string `json:"actor_id"`
}

// GetTypename returns AuthActorAuthActorUser.Typename, and is useful for accessing the field via an interface.
func (v *AuthActorAuthActorUser) GetTypename() string { return v.Typename }

// GetActor_type returns AuthActorAuthActorUser.Actor_type, and is useful for accessing the field via an interface.
func (v *AuthActorAuthActorUser) GetActor_type() string { return v.Actor_type }

// GetActor_id returns AuthActorAuthActorUser.Actor_id, and is useful for accessing the field via an interface.
func (v *AuthActorAuthActorUser) GetActor_id() string { return v.Actor_id }

// AuthActorResponse is returned by AuthActor on success.
type AuthActorResponse struct {
	Auth AuthActorAuth `json:"auth"`
}

// GetAuth returns AuthActorResponse.Auth, and is useful for accessing the field via an interface.
func (v *AuthActorResponse) GetAuth() AuthActorAuth { return v.Auth }

type FlowInput struct {
	Name  string          `json:"name"`
	Metas []FlowMetaInput `json:"metas"`
}

// GetName returns FlowInput.Name, and is useful for accessing the field via an interface.
func (v *FlowInput) GetName() string { return v.Name }

// GetMetas returns FlowInput.Metas, and is useful for accessing the field via an interface.
func (v *FlowInput) GetMetas() []FlowMetaInput { return v.Metas }

type FlowMetaInput struct {
	Key   string `json:"key"`
	Value string `json:"value"`
}

// GetKey returns FlowMetaInput.Key, and is useful for accessing the field via an interface.
func (v *FlowMetaInput) GetKey() string { return v.Key }

// GetValue returns FlowMetaInput.Value, and is useful for accessing the field via an interface.
func (v *FlowMetaInput) GetValue() string { return v.Value }

type InvocationEndInput struct {
	InvocationId string `json:"invocationId"`
	Error        bool   `json:"error"`
}

// GetInvocationId returns InvocationEndInput.InvocationId, and is useful for accessing the field via an interface.
func (v *InvocationEndInput) GetInvocationId() string { return v.InvocationId }

// GetError returns InvocationEndInput.Error, and is useful for accessing the field via an interface.
func (v *InvocationEndInput) GetError() bool { return v.Error }

type InvocationInput struct {
	Args      []string        `json:"args"`
	Config    json.RawMessage `json:"config"`
	StartTime time.Time       `json:"startTime"`
}

// GetArgs returns InvocationInput.Args, and is useful for accessing the field via an interface.
func (v *InvocationInput) GetArgs() []string { return v.Args }

// GetConfig returns InvocationInput.Config, and is useful for accessing the field via an interface.
func (v *InvocationInput) GetConfig() json.RawMessage { return v.Config }

// GetStartTime returns InvocationInput.StartTime, and is useful for accessing the field via an interface.
func (v *InvocationInput) GetStartTime() time.Time { return v.StartTime }

// LoginLoginLoginPayload includes the requested fields of the GraphQL type LoginPayload.
type LoginLoginLoginPayload struct {
	Token string                     `json:"token"`
	User  LoginLoginLoginPayloadUser `json:"user"`
}

// GetToken returns LoginLoginLoginPayload.Token, and is useful for accessing the field via an interface.
func (v *LoginLoginLoginPayload) GetToken() string { return v.Token }

// GetUser returns LoginLoginLoginPayload.User, and is useful for accessing the field via an interface.
func (v *LoginLoginLoginPayload) GetUser() LoginLoginLoginPayloadUser { return v.User }

// LoginLoginLoginPayloadUser includes the requested fields of the GraphQL type User.
type LoginLoginLoginPayloadUser struct {
	Id    string `json:"id"`
	Email string `json:"email"`
}

// GetId returns LoginLoginLoginPayloadUser.Id, and is useful for accessing the field via an interface.
func (v *LoginLoginLoginPayloadUser) GetId() string { return v.Id }

// GetEmail returns LoginLoginLoginPayloadUser.Email, and is useful for accessing the field via an interface.
func (v *LoginLoginLoginPayloadUser) GetEmail() string { return v.Email }

// LoginResponse is returned by Login on success.
type LoginResponse struct {
	Login LoginLoginLoginPayload `json:"login"`
}

// GetLogin returns LoginResponse.Login, and is useful for accessing the field via an interface.
func (v *LoginResponse) GetLogin() LoginLoginLoginPayload { return v.Login }

// RegisterFlowInvocationRegisterFlowInvocationFlow includes the requested fields of the GraphQL type Flow.
type RegisterFlowInvocationRegisterFlowInvocationFlow struct {
	Id          string                                                                          `json:"id"`
	Invocations RegisterFlowInvocationRegisterFlowInvocationFlowInvocationsInvocationConnection `json:"invocations"`
}

// GetId returns RegisterFlowInvocationRegisterFlowInvocationFlow.Id, and is useful for accessing the field via an interface.
func (v *RegisterFlowInvocationRegisterFlowInvocationFlow) GetId() string { return v.Id }

// GetInvocations returns RegisterFlowInvocationRegisterFlowInvocationFlow.Invocations, and is useful for accessing the field via an interface.
func (v *RegisterFlowInvocationRegisterFlowInvocationFlow) GetInvocations() RegisterFlowInvocationRegisterFlowInvocationFlowInvocationsInvocationConnection {
	return v.Invocations
}

// RegisterFlowInvocationRegisterFlowInvocationFlowInvocationsInvocationConnection includes the requested fields of the GraphQL type InvocationConnection.
type RegisterFlowInvocationRegisterFlowInvocationFlowInvocationsInvocationConnection struct {
	Edges []RegisterFlowInvocationRegisterFlowInvocationFlowInvocationsInvocationConnectionEdgesInvocationEdge `json:"edges"`
}

// GetEdges returns RegisterFlowInvocationRegisterFlowInvocationFlowInvocationsInvocationConnection.Edges, and is useful for accessing the field via an interface.
func (v *RegisterFlowInvocationRegisterFlowInvocationFlowInvocationsInvocationConnection) GetEdges() []RegisterFlowInvocationRegisterFlowInvocationFlowInvocationsInvocationConnectionEdgesInvocationEdge {
	return v.Edges
}

// RegisterFlowInvocationRegisterFlowInvocationFlowInvocationsInvocationConnectionEdgesInvocationEdge includes the requested fields of the GraphQL type InvocationEdge.
type RegisterFlowInvocationRegisterFlowInvocationFlowInvocationsInvocationConnectionEdgesInvocationEdge struct {
	Node RegisterFlowInvocationRegisterFlowInvocationFlowInvocationsInvocationConnectionEdgesInvocationEdgeNodeInvocation `json:"node"`
}

// GetNode returns RegisterFlowInvocationRegisterFlowInvocationFlowInvocationsInvocationConnectionEdgesInvocationEdge.Node, and is useful for accessing the field via an interface.
func (v *RegisterFlowInvocationRegisterFlowInvocationFlowInvocationsInvocationConnectionEdgesInvocationEdge) GetNode() RegisterFlowInvocationRegisterFlowInvocationFlowInvocationsInvocationConnectionEdgesInvocationEdgeNodeInvocation {
	return v.Node
}

// RegisterFlowInvocationRegisterFlowInvocationFlowInvocationsInvocationConnectionEdgesInvocationEdgeNodeInvocation includes the requested fields of the GraphQL type Invocation.
type RegisterFlowInvocationRegisterFlowInvocationFlowInvocationsInvocationConnectionEdgesInvocationEdgeNodeInvocation struct {
	Id string `json:"id"`
}

// GetId returns RegisterFlowInvocationRegisterFlowInvocationFlowInvocationsInvocationConnectionEdgesInvocationEdgeNodeInvocation.Id, and is useful for accessing the field via an interface.
func (v *RegisterFlowInvocationRegisterFlowInvocationFlowInvocationsInvocationConnectionEdgesInvocationEdgeNodeInvocation) GetId() string {
	return v.Id
}

// RegisterFlowInvocationResponse is returned by RegisterFlowInvocation on success.
type RegisterFlowInvocationResponse struct {
	RegisterFlowInvocation RegisterFlowInvocationRegisterFlowInvocationFlow `json:"registerFlowInvocation"`
}

// GetRegisterFlowInvocation returns RegisterFlowInvocationResponse.RegisterFlowInvocation, and is useful for accessing the field via an interface.
func (v *RegisterFlowInvocationResponse) GetRegisterFlowInvocation() RegisterFlowInvocationRegisterFlowInvocationFlow {
	return v.RegisterFlowInvocation
}

// RegisterFlowRegisterFlow includes the requested fields of the GraphQL type Flow.
type RegisterFlowRegisterFlow struct {
	Id string `json:"id"`
}

// GetId returns RegisterFlowRegisterFlow.Id, and is useful for accessing the field via an interface.
func (v *RegisterFlowRegisterFlow) GetId() string { return v.Id }

// RegisterFlowResponse is returned by RegisterFlow on success.
type RegisterFlowResponse struct {
	RegisterFlow RegisterFlowRegisterFlow `json:"registerFlow"`
}

// GetRegisterFlow returns RegisterFlowResponse.RegisterFlow, and is useful for accessing the field via an interface.
func (v *RegisterFlowResponse) GetRegisterFlow() RegisterFlowRegisterFlow { return v.RegisterFlow }

// RegisterInvocationRegisterInvocation includes the requested fields of the GraphQL type Invocation.
type RegisterInvocationRegisterInvocation struct {
	Id string `json:"id"`
}

// GetId returns RegisterInvocationRegisterInvocation.Id, and is useful for accessing the field via an interface.
func (v *RegisterInvocationRegisterInvocation) GetId() string { return v.Id }

// RegisterInvocationResponse is returned by RegisterInvocation on success.
type RegisterInvocationResponse struct {
	RegisterInvocation RegisterInvocationRegisterInvocation `json:"registerInvocation"`
}

// GetRegisterInvocation returns RegisterInvocationResponse.RegisterInvocation, and is useful for accessing the field via an interface.
func (v *RegisterInvocationResponse) GetRegisterInvocation() RegisterInvocationRegisterInvocation {
	return v.RegisterInvocation
}

// SendEndInvocationEndInvocation includes the requested fields of the GraphQL type Invocation.
type SendEndInvocationEndInvocation struct {
	Id string `json:"id"`
}

// GetId returns SendEndInvocationEndInvocation.Id, and is useful for accessing the field via an interface.
func (v *SendEndInvocationEndInvocation) GetId() string { return v.Id }

// SendEndInvocationResponse is returned by SendEndInvocation on success.
type SendEndInvocationResponse struct {
	EndInvocation SendEndInvocationEndInvocation `json:"endInvocation"`
}

// GetEndInvocation returns SendEndInvocationResponse.EndInvocation, and is useful for accessing the field via an interface.
func (v *SendEndInvocationResponse) GetEndInvocation() SendEndInvocationEndInvocation {
	return v.EndInvocation
}

// SendEventsIngestSpansTargetSpan includes the requested fields of the GraphQL type TargetSpan.
type SendEventsIngestSpansTargetSpan struct {
	Id     string `json:"id"`
	SpanId string `json:"spanId"`
}

// GetId returns SendEventsIngestSpansTargetSpan.Id, and is useful for accessing the field via an interface.
func (v *SendEventsIngestSpansTargetSpan) GetId() string { return v.Id }

// GetSpanId returns SendEventsIngestSpansTargetSpan.SpanId, and is useful for accessing the field via an interface.
func (v *SendEventsIngestSpansTargetSpan) GetSpanId() string { return v.SpanId }

// SendEventsResponse is returned by SendEvents on success.
type SendEventsResponse struct {
	IngestSpans []SendEventsIngestSpansTargetSpan `json:"ingestSpans"`
}

// GetIngestSpans returns SendEventsResponse.IngestSpans, and is useful for accessing the field via an interface.
func (v *SendEventsResponse) GetIngestSpans() []SendEventsIngestSpansTargetSpan { return v.IngestSpans }

// SendInvocationHeartbeatHeartbeatInvocation includes the requested fields of the GraphQL type Invocation.
type SendInvocationHeartbeatHeartbeatInvocation struct {
	Id string `json:"id"`
}

// GetId returns SendInvocationHeartbeatHeartbeatInvocation.Id, and is useful for accessing the field via an interface.
func (v *SendInvocationHeartbeatHeartbeatInvocation) GetId() string { return v.Id }

// SendInvocationHeartbeatResponse is returned by SendInvocationHeartbeat on success.
type SendInvocationHeartbeatResponse struct {
	HeartbeatInvocation SendInvocationHeartbeatHeartbeatInvocation `json:"heartbeatInvocation"`
}

// GetHeartbeatInvocation returns SendInvocationHeartbeatResponse.HeartbeatInvocation, and is useful for accessing the field via an interface.
func (v *SendInvocationHeartbeatResponse) GetHeartbeatInvocation() SendInvocationHeartbeatHeartbeatInvocation {
	return v.HeartbeatInvocation
}

// SendLogsResponse is returned by SendLogs on success.
type SendLogsResponse struct {
	IngestSpanLogs string `json:"ingestSpanLogs"`
}

// GetIngestSpanLogs returns SendLogsResponse.IngestSpanLogs, and is useful for accessing the field via an interface.
func (v *SendLogsResponse) GetIngestSpanLogs() string { return v.IngestSpanLogs }

// __LoginInput is used internally by genqlient
type __LoginInput struct {
	Email string `json:"email"`
	Pass  string `json:"pass"`
}

// GetEmail returns __LoginInput.Email, and is useful for accessing the field via an interface.
func (v *__LoginInput) GetEmail() string { return v.Email }

// GetPass returns __LoginInput.Pass, and is useful for accessing the field via an interface.
func (v *__LoginInput) GetPass() string { return v.Pass }

// __RegisterFlowInput is used internally by genqlient
type __RegisterFlowInput struct {
	ProjectId string    `json:"projectId"`
	Flow      FlowInput `json:"flow"`
}

// GetProjectId returns __RegisterFlowInput.ProjectId, and is useful for accessing the field via an interface.
func (v *__RegisterFlowInput) GetProjectId() string { return v.ProjectId }

// GetFlow returns __RegisterFlowInput.Flow, and is useful for accessing the field via an interface.
func (v *__RegisterFlowInput) GetFlow() FlowInput { return v.Flow }

// __RegisterFlowInvocationInput is used internally by genqlient
type __RegisterFlowInvocationInput struct {
	ProjectId  string          `json:"projectId"`
	Flow       FlowInput       `json:"flow"`
	Invocation InvocationInput `json:"invocation"`
}

// GetProjectId returns __RegisterFlowInvocationInput.ProjectId, and is useful for accessing the field via an interface.
func (v *__RegisterFlowInvocationInput) GetProjectId() string { return v.ProjectId }

// GetFlow returns __RegisterFlowInvocationInput.Flow, and is useful for accessing the field via an interface.
func (v *__RegisterFlowInvocationInput) GetFlow() FlowInput { return v.Flow }

// GetInvocation returns __RegisterFlowInvocationInput.Invocation, and is useful for accessing the field via an interface.
func (v *__RegisterFlowInvocationInput) GetInvocation() InvocationInput { return v.Invocation }

// __RegisterInvocationInput is used internally by genqlient
type __RegisterInvocationInput struct {
	FlowId     string          `json:"flowId"`
	Invocation InvocationInput `json:"invocation"`
}

// GetFlowId returns __RegisterInvocationInput.FlowId, and is useful for accessing the field via an interface.
func (v *__RegisterInvocationInput) GetFlowId() string { return v.FlowId }

// GetInvocation returns __RegisterInvocationInput.Invocation, and is useful for accessing the field via an interface.
func (v *__RegisterInvocationInput) GetInvocation() InvocationInput { return v.Invocation }

// __SendEndInvocationInput is used internally by genqlient
type __SendEndInvocationInput struct {
	Idata InvocationEndInput `json:"idata"`
}

// GetIdata returns __SendEndInvocationInput.Idata, and is useful for accessing the field via an interface.
func (v *__SendEndInvocationInput) GetIdata() InvocationEndInput { return v.Idata }

// __SendEventsInput is used internally by genqlient
type __SendEventsInput struct {
	InvocationId string            `json:"invocationId"`
	Spans        []json.RawMessage `json:"spans"`
}

// GetInvocationId returns __SendEventsInput.InvocationId, and is useful for accessing the field via an interface.
func (v *__SendEventsInput) GetInvocationId() string { return v.InvocationId }

// GetSpans returns __SendEventsInput.Spans, and is useful for accessing the field via an interface.
func (v *__SendEventsInput) GetSpans() []json.RawMessage { return v.Spans }

// __SendInvocationHeartbeatInput is used internally by genqlient
type __SendInvocationHeartbeatInput struct {
	InvocationId string `json:"invocationId"`
}

// GetInvocationId returns __SendInvocationHeartbeatInput.InvocationId, and is useful for accessing the field via an interface.
func (v *__SendInvocationHeartbeatInput) GetInvocationId() string { return v.InvocationId }

// __SendLogsInput is used internally by genqlient
type __SendLogsInput struct {
	SpanId string `json:"spanId"`
	Bdata  string `json:"bdata"`
}

// GetSpanId returns __SendLogsInput.SpanId, and is useful for accessing the field via an interface.
func (v *__SendLogsInput) GetSpanId() string { return v.SpanId }

// GetBdata returns __SendLogsInput.Bdata, and is useful for accessing the field via an interface.
func (v *__SendLogsInput) GetBdata() string { return v.Bdata }

func AuthActor(
	ctx context.Context,
	client graphql.Client,
) (*AuthActorResponse, error) {
	req := &graphql.Request{
		OpName: "AuthActor",
		Query: `
query AuthActor {
	auth {
		actor {
			__typename
			actor_type
			actor_id
		}
	}
}
`,
	}
	var err error

	var data AuthActorResponse
	resp := &graphql.Response{Data: &data}

	err = client.MakeRequest(
		ctx,
		req,
		resp,
	)

	return &data, err
}

func Login(
	ctx context.Context,
	client graphql.Client,
	email string,
	pass string,
) (*LoginResponse, error) {
	req := &graphql.Request{
		OpName: "Login",
		Query: `
mutation Login ($email: String!, $pass: String!) {
	login(email: $email, password: $pass) {
		token
		user {
			id
			email
		}
	}
}
`,
		Variables: &__LoginInput{
			Email: email,
			Pass:  pass,
		},
	}
	var err error

	var data LoginResponse
	resp := &graphql.Response{Data: &data}

	err = client.MakeRequest(
		ctx,
		req,
		resp,
	)

	return &data, err
}

func RegisterFlow(
	ctx context.Context,
	client graphql.Client,
	projectId string,
	flow FlowInput,
) (*RegisterFlowResponse, error) {
	req := &graphql.Request{
		OpName: "RegisterFlow",
		Query: `
mutation RegisterFlow ($projectId: ID!, $flow: FlowInput!) {
	registerFlow(projectId: $projectId, flow: $flow) {
		id
	}
}
`,
		Variables: &__RegisterFlowInput{
			ProjectId: projectId,
			Flow:      flow,
		},
	}
	var err error

	var data RegisterFlowResponse
	resp := &graphql.Response{Data: &data}

	err = client.MakeRequest(
		ctx,
		req,
		resp,
	)

	return &data, err
}

func RegisterFlowInvocation(
	ctx context.Context,
	client graphql.Client,
	projectId string,
	flow FlowInput,
	invocation InvocationInput,
) (*RegisterFlowInvocationResponse, error) {
	req := &graphql.Request{
		OpName: "RegisterFlowInvocation",
		Query: `
mutation RegisterFlowInvocation ($projectId: ID!, $flow: FlowInput!, $invocation: InvocationInput!) {
	registerFlowInvocation(projectId: $projectId, flow: $flow, invocation: $invocation) {
		id
		invocations {
			edges {
				node {
					id
				}
			}
		}
	}
}
`,
		Variables: &__RegisterFlowInvocationInput{
			ProjectId:  projectId,
			Flow:       flow,
			Invocation: invocation,
		},
	}
	var err error

	var data RegisterFlowInvocationResponse
	resp := &graphql.Response{Data: &data}

	err = client.MakeRequest(
		ctx,
		req,
		resp,
	)

	return &data, err
}

func RegisterInvocation(
	ctx context.Context,
	client graphql.Client,
	flowId string,
	invocation InvocationInput,
) (*RegisterInvocationResponse, error) {
	req := &graphql.Request{
		OpName: "RegisterInvocation",
		Query: `
mutation RegisterInvocation ($flowId: ID!, $invocation: InvocationInput!) {
	registerInvocation(flowId: $flowId, invocation: $invocation) {
		id
	}
}
`,
		Variables: &__RegisterInvocationInput{
			FlowId:     flowId,
			Invocation: invocation,
		},
	}
	var err error

	var data RegisterInvocationResponse
	resp := &graphql.Response{Data: &data}

	err = client.MakeRequest(
		ctx,
		req,
		resp,
	)

	return &data, err
}

func SendEndInvocation(
	ctx context.Context,
	client graphql.Client,
	idata InvocationEndInput,
) (*SendEndInvocationResponse, error) {
	req := &graphql.Request{
		OpName: "SendEndInvocation",
		Query: `
mutation SendEndInvocation ($idata: InvocationEndInput!) {
	endInvocation(data: $idata) {
		id
	}
}
`,
		Variables: &__SendEndInvocationInput{
			Idata: idata,
		},
	}
	var err error

	var data SendEndInvocationResponse
	resp := &graphql.Response{Data: &data}

	err = client.MakeRequest(
		ctx,
		req,
		resp,
	)

	return &data, err
}

func SendEvents(
	ctx context.Context,
	client graphql.Client,
	invocationId string,
	spans []json.RawMessage,
) (*SendEventsResponse, error) {
	req := &graphql.Request{
		OpName: "SendEvents",
		Query: `
mutation SendEvents ($invocationId: ID!, $spans: [JSON!]!) {
	ingestSpans(invocationId: $invocationId, spans: $spans) {
		id
		spanId
	}
}
`,
		Variables: &__SendEventsInput{
			InvocationId: invocationId,
			Spans:        spans,
		},
	}
	var err error

	var data SendEventsResponse
	resp := &graphql.Response{Data: &data}

	err = client.MakeRequest(
		ctx,
		req,
		resp,
	)

	return &data, err
}

func SendInvocationHeartbeat(
	ctx context.Context,
	client graphql.Client,
	invocationId string,
) (*SendInvocationHeartbeatResponse, error) {
	req := &graphql.Request{
		OpName: "SendInvocationHeartbeat",
		Query: `
mutation SendInvocationHeartbeat ($invocationId: ID!) {
	heartbeatInvocation(invocationId: $invocationId) {
		id
	}
}
`,
		Variables: &__SendInvocationHeartbeatInput{
			InvocationId: invocationId,
		},
	}
	var err error

	var data SendInvocationHeartbeatResponse
	resp := &graphql.Response{Data: &data}

	err = client.MakeRequest(
		ctx,
		req,
		resp,
	)

	return &data, err
}

func SendLogs(
	ctx context.Context,
	client graphql.Client,
	spanId string,
	bdata string,
) (*SendLogsResponse, error) {
	req := &graphql.Request{
		OpName: "SendLogs",
		Query: `
mutation SendLogs ($spanId: ID!, $bdata: Bytes!) {
	ingestSpanLogs(spanId: $spanId, data: $bdata)
}
`,
		Variables: &__SendLogsInput{
			SpanId: spanId,
			Bdata:  bdata,
		},
	}
	var err error

	var data SendLogsResponse
	resp := &graphql.Response{Data: &data}

	err = client.MakeRequest(
		ctx,
		req,
		resp,
	)

	return &data, err
}