// Copyright 2026 Alibaba Group Holding Ltd.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.

package opensandbox

import (
	"context"
	"encoding/json"
	"errors"
	"reflect"
	"testing"
)

func TestCreateSandboxRequestIsolationJSON(t *testing.T) {
	request := CreateSandboxRequest{
		Extensions: map[string]string{"poolRef": "projects"},
		Isolation: &SandboxIsolation{
			Type: "bwrap",
			Mounts: []SandboxIsolationMount{{
				Root: "projects", SubPath: "project-A", Target: "/workspace/a", Mode: "rw",
			}},
		},
	}

	data, err := json.Marshal(request)
	if err != nil {
		t.Fatalf("marshal create request: %v", err)
	}
	wantJSON := `{
		"extensions": {"poolRef": "projects"},
		"isolation": {
			"type": "bwrap",
			"mounts": [{
				"root": "projects",
				"subPath": "project-A",
				"target": "/workspace/a",
				"mode": "rw"
			}]
		}
	}`
	var got, want map[string]any
	if err := json.Unmarshal(data, &got); err != nil {
		t.Fatalf("decode marshalled request: %v", err)
	}
	if err := json.Unmarshal([]byte(wantJSON), &want); err != nil {
		t.Fatalf("decode expected request: %v", err)
	}
	if !reflect.DeepEqual(got, want) {
		t.Fatalf("unexpected request JSON:\n got: %s\nwant: %s", data, wantJSON)
	}
}

func TestCreateSandboxRejectsIsolationWithoutPool(t *testing.T) {
	_, err := CreateSandbox(context.Background(), ConnectionConfig{}, SandboxCreateOptions{
		Image:     "python:3.13",
		Isolation: &SandboxIsolation{Type: "bwrap"},
	})
	var invalid *InvalidArgumentError
	if !errors.As(err, &invalid) || invalid.Field != "Isolation" {
		t.Fatalf("expected Isolation invalid-argument error, got %v", err)
	}
}
