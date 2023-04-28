package cloudclient

import (
	"github.com/Khan/genqlient/graphql"
	"net"
	"net/http"
	"time"
)

type HephClient struct {
	Url string
	graphql.Client
}

func newClient() *http.Client {
	var netTransport = &http.Transport{
		DialContext: (&net.Dialer{
			Timeout: 5 * time.Second,
		}).DialContext,
		TLSHandshakeTimeout: 5 * time.Second,
	}
	return &http.Client{
		Timeout:   time.Second * 30,
		Transport: netTransport,
	}
}

func (c HephClient) WithAuthToken(tok string) HephClient {
	httpClient := newClient()
	httpClient.Transport = &authedTransport{
		token:   tok,
		wrapped: httpClient.Transport,
	}

	return HephClient{
		Url:    c.Url,
		Client: graphql.NewClient(c.Url, httpClient),
	}
}

type authedTransport struct {
	token   string
	wrapped http.RoundTripper
}

func (t *authedTransport) RoundTrip(req *http.Request) (*http.Response, error) {
	req.Header.Set("Authorization", "Bearer "+t.token)
	return t.wrapped.RoundTrip(req)
}

func New(url string) HephClient {
	return HephClient{
		Url:    url,
		Client: graphql.NewClient(url, newClient()),
	}
}
