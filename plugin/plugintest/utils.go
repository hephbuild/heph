package plugintest

import (
	"net/http/httptest"
	"testing"

	"github.com/hephbuild/heph/plugin/gen/heph/plugin/v1/pluginv1connect"
)

func ProviderClient(t *testing.T, p pluginv1connect.ProviderHandler) pluginv1connect.ProviderClient {
	_, h := pluginv1connect.NewProviderHandler(p)

	srv := httptest.NewServer(h)
	t.Cleanup(srv.Close)

	return pluginv1connect.NewProviderClient(srv.Client(), srv.URL)
}
