// Copyright 2026 Alibaba Group Holding Ltd.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.

package controller

import (
	"testing"

	"github.com/stretchr/testify/require"
	corev1 "k8s.io/api/core/v1"
	metav1 "k8s.io/apimachinery/pkg/apis/meta/v1"

	sandboxv1alpha1 "github.com/alibaba/OpenSandbox/sandbox-k8s/apis/sandbox/v1alpha1"
)

func TestPreparePoolControlSecret(t *testing.T) {
	pod := &corev1.Pod{
		ObjectMeta: metav1.ObjectMeta{GenerateName: "secure-"},
		Spec:       corev1.PodSpec{Containers: []corev1.Container{{Name: "sandbox"}}},
	}
	pool := &sandboxv1alpha1.Pool{ObjectMeta: metav1.ObjectMeta{Name: "secure"}}

	secret, err := preparePoolControlSecret(pod, pool)
	require.NoError(t, err)
	require.NotEmpty(t, pod.Name)
	require.Empty(t, pod.GenerateName)
	require.Equal(t, secret.Name, pod.Annotations[controlTokenSecretAnnotation])
	require.Len(t, secret.Data["execd-token"], 43)
	require.Len(t, secret.Data["task-executor-token"], 43)
	require.NotEqual(t, secret.Data["execd-token"], secret.Data["task-executor-token"])

	env := pod.Spec.Containers[0].Env
	require.Len(t, env, 2)
	for _, item := range env {
		require.NotNil(t, item.ValueFrom)
		require.NotNil(t, item.ValueFrom.SecretKeyRef)
		require.Equal(t, secret.Name, item.ValueFrom.SecretKeyRef.Name)
	}
}

func TestPreparePoolControlSecretScopesSidecarCredentials(t *testing.T) {
	pod := &corev1.Pod{Spec: corev1.PodSpec{Containers: []corev1.Container{
		{Name: "task-executor"},
		{Name: "sandbox"},
		{Name: "metrics"},
	}}}
	pool := &sandboxv1alpha1.Pool{ObjectMeta: metav1.ObjectMeta{Name: "secure"}}

	_, err := preparePoolControlSecret(pod, pool)
	require.NoError(t, err)
	require.Equal(t, "TASK_EXECUTOR_AUTH_TOKEN", pod.Spec.Containers[0].Env[0].Name)
	require.Equal(t, "EXECD_ACCESS_TOKEN", pod.Spec.Containers[1].Env[0].Name)
	require.Empty(t, pod.Spec.Containers[2].Env)
}
