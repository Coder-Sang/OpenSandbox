#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <linux/bpf.h>
#include <linux/capability.h>
#include <linux/keyctl.h>
#include <sched.h>
#include <stdio.h>
#include <sys/mount.h>
#include <sys/prctl.h>
#include <sys/ptrace.h>
#include <sys/syscall.h>
#include <unistd.h>

static void result(const char *name, long rc) {
    if (rc == -1) {
        printf("%s=blocked errno=%d\n", name, errno);
        return;
    }
    printf("%s=ALLOWED rc=%ld\n", name, rc);
}

int main(void) {
    int nsfd;
	struct __user_cap_header_struct cap_header = {
		.version = _LINUX_CAPABILITY_VERSION_3,
		.pid = 0,
	};
	struct __user_cap_data_struct cap_data[2] = {{0}};
	long nnp;
	long seccomp;

	if (syscall(SYS_capget, &cap_header, &cap_data) == 0) {
		printf("uid=%u gid=%u\n", (unsigned)getuid(), (unsigned)getgid());
		printf("CapInh=%08x%08x\n", cap_data[1].inheritable, cap_data[0].inheritable);
		printf("CapPrm=%08x%08x\n", cap_data[1].permitted, cap_data[0].permitted);
		printf("CapEff=%08x%08x\n", cap_data[1].effective, cap_data[0].effective);
	} else {
		printf("capget=ERROR errno=%d\n", errno);
	}
	nnp = prctl(PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0);
	seccomp = prctl(PR_GET_SECCOMP, 0, 0, 0, 0);
	printf("NoNewPrivs=%ld Seccomp=%ld\n", nnp, seccomp);
	/* Bounding and ambient sets are queried one capability at a time. */
	{
		unsigned long long bounding = 0;
		unsigned long long ambient = 0;
		for (int cap = 0; cap < 64; cap++) {
			if (prctl(PR_CAPBSET_READ, cap, 0, 0, 0) == 1)
				bounding |= 1ULL << cap;
			if (prctl(PR_CAP_AMBIENT, PR_CAP_AMBIENT_IS_SET, cap, 0, 0) == 1)
				ambient |= 1ULL << cap;
		}
		printf("CapBnd=%016llx\nCapAmb=%016llx\n", bounding, ambient);
	}

    errno = 0;
    result("mount", mount("none", "/tmp", "tmpfs", 0, NULL));
    errno = 0;
    result("umount2", umount2("/tmp", 0));
    errno = 0;
    result("pivot_root", syscall(SYS_pivot_root, "/", "/"));
    errno = 0;
    result("chroot", chroot("/"));

    nsfd = open("/proc/self/ns/mnt", O_RDONLY | O_CLOEXEC);
    errno = 0;
    result("setns", nsfd < 0 ? -1 : setns(nsfd, 0));
    if (nsfd >= 0) {
        close(nsfd);
    }

    errno = 0;
    result("unshare", unshare(CLONE_NEWNS));
    errno = 0;
    result("ptrace", ptrace(PTRACE_ATTACH, 1, NULL, NULL));
    errno = 0;
    result("bpf", syscall(SYS_bpf, BPF_MAP_CREATE, NULL, 0));
    errno = 0;
    result("keyctl", syscall(SYS_keyctl, KEYCTL_GET_KEYRING_ID, 0, 0));
    return 0;
}
