// Copyright 2026 The OpenSandbox Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

package runtime

import (
	"net/http"
	"net/http/httptest"
	"testing"

	"github.com/alibaba/opensandbox/execd/pkg/jupyter"
	"github.com/stretchr/testify/require"
)

func TestSearchKernel(t *testing.T) {
	tests := []struct {
		name, body string
		language   Language
		want       string
		wantError  bool
	}{
		{"python3 fallback", `{"kernelspecs":{"python3":{"spec":{"language":"python"}}}}`, Python, "python3", false},
		{"prefer alternate deterministically", `{"kernelspecs":{"python3":{"spec":{"language":"python"}},"z-python":{"spec":{"language":"python"}},"a-python":{"spec":{"language":"python"}}}}`, Python, "a-python", false},
		{"ignore incompatible alternate", `{"kernelspecs":{"python3":{"spec":{"language":"python"}},"java":{"spec":{"language":"java"}}}}`, Python, "python3", false},
		{"other language", `{"kernelspecs":{"python3":{"spec":{"language":"python"}},"java":{"spec":{"language":"java"}}}}`, Java, "java", false},
		{"no language match", `{"kernelspecs":{"python3":{"spec":{"language":"python"}}}}`, Java, "", true},
		{"empty specs", `{"kernelspecs":{}}`, Python, "", true},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
				require.Equal(t, "/api/kernelspecs", r.URL.Path)
				w.Header().Set("Content-Type", "application/json")
				_, _ = w.Write([]byte(tt.body))
			}))
			defer server.Close()
			client := jupyter.NewClient(server.URL)
			for i := 0; i < 30; i++ {
				got, err := (&Controller{}).searchKernel(client, tt.language)
				if tt.wantError {
					require.Error(t, err)
				} else {
					require.NoError(t, err)
				}
				require.Equal(t, tt.want, got)
			}
		})
	}
}
