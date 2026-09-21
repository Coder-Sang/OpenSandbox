/*
 * Copyright 2026 Alibaba Group Holding Ltd.
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

/*
 * opensandbox-nsenter is execd's private pool-runtime launch trampoline.
 * It consumes execd-pinned namespace descriptors, enters the long-lived
 * bwrap workload, forks (required for PID namespaces), and execs the requested
 * command. It is never bind-mounted into the user namespace.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <sched.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

#define NS_COUNT 6
#define EXIT_USAGE 125
#define EXIT_EXEC 126

static pid_t child_pid = -1;

static void forward_signal(int sig)
{
    if (child_pid > 0)
        (void)kill(child_pid, sig);
}

static void die(const char *what)
{
    fprintf(stderr, "opensandbox-nsenter: %s: %s\n", what, strerror(errno));
    _exit(EXIT_USAGE);
}

int main(int argc, char **argv)
{
    int fds[NS_COUNT];
    char *end = NULL;
    int separator = 8;
    int status;

    if (argc < 10 || strcmp(argv[separator], "--") != 0) {
        fprintf(stderr, "usage: opensandbox-nsenter USERFD MNTFD IPCFD UTSFD CGROUPFD PIDFD CWD -- COMMAND [ARG...]\n");
        return EXIT_USAGE;
    }

    for (int i = 0; i < NS_COUNT; i++) {
        if (strcmp(argv[i + 1], "-") == 0) {
            fds[i] = -1;
            continue;
        }
        errno = 0;
        long fd = strtol(argv[i + 1], &end, 10);
        if (errno != 0 || end == argv[i + 1] || *end != '\0' || fd < 0)
            die("invalid namespace descriptor");
        fds[i] = (int)fd;
    }

    for (int i = 0; i < NS_COUNT; i++) {
        if (fds[i] < 0)
            continue;
        if (setns(fds[i], 0) != 0)
            die("setns");
        close(fds[i]);
    }

    child_pid = fork();
    if (child_pid < 0)
        die("fork");
    if (child_pid == 0) {
        if (strcmp(argv[7], "-") != 0 && chdir(argv[7]) != 0)
            die("chdir");
        execvp(argv[separator + 1], &argv[separator + 1]);
        die("exec");
    }

    for (int sig = 1; sig < NSIG; sig++) {
        if (sig == SIGKILL || sig == SIGSTOP || sig == SIGCHLD)
            continue;
        struct sigaction sa;
        memset(&sa, 0, sizeof(sa));
        sa.sa_handler = forward_signal;
        sigemptyset(&sa.sa_mask);
        (void)sigaction(sig, &sa, NULL);
    }

    while (waitpid(child_pid, &status, 0) < 0) {
        if (errno != EINTR)
            die("waitpid");
    }
    if (WIFEXITED(status))
        return WEXITSTATUS(status);
    if (WIFSIGNALED(status)) {
        signal(WTERMSIG(status), SIG_DFL);
        raise(WTERMSIG(status));
    }
    return EXIT_EXEC;
}
