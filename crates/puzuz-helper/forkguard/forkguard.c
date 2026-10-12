// SPDX-License-Identifier: MIT
/*
 * puzuz-forkguard: preloaded into every puzuz job (LD_PRELOAD).
 *
 * A process that has initialized CUDA holds file descriptors and memory
 * mappings of the NVIDIA device files (/dev/nvidiactl, /dev/nvidia<N>,
 * /dev/nvidia-uvm). A child created with fork() without exec, such as a
 * PyTorch DataLoader worker with the default "fork" start method, inherits
 * them. CUDA cannot be used in such a child, so it never touches them, but
 * cuda-checkpoint only releases them in processes that have a CUDA context
 * and CRIU cannot dump them ("Can't handle non-regular mapping", or an
 * unsupported character device fd), so the whole job could not be
 * checkpointed.
 *
 * This library interposes fork(). In the child, after fork() (and all
 * pthread_atfork child handlers) returned, it unmaps every mapping of a
 * /dev/nvidia* file and closes every descriptor of one. That is what the
 * kernel does anyway when the child exits, it does not affect the parent
 * (descriptors and mappings are per process), and it is a no-op in a
 * process without such mappings.
 *
 * The child of a multithreaded parent may only use async-signal-safe
 * functions, so the cleanup uses plain system calls and static buffers
 * (no malloc, no stdio).
 *
 * Not covered (documented in the helper): statically linked programs and
 * programs that bypass the dynamic fork() symbol (raw clone(2), _Fork(),
 * vfork()/posix_spawn(), which exec right away and lose the mappings then).
 */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <fcntl.h>
#include <stddef.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <unistd.h>

#define EXPORT __attribute__((visibility("default")))

/* PUZUZ_FORKGUARD_TEST_PREFIX replaces it in tests (no NVIDIA devices
 * there). It only affects the process that sets it. */
static char prefix[256] = "/dev/nvidia";
static size_t prefix_len = sizeof("/dev/nvidia") - 1;

/* Collected before unmapping (unmapping while reading the maps file would
 * make the read skip or repeat lines); rescanned when full. */
#define MAX_RANGES 256
#define MAX_PASSES 64

static pid_t (*real_fork)(void);

static int is_device_path(const char *p, size_t n)
{
    if (n < prefix_len)
        return 0;
    for (size_t i = 0; i < prefix_len; i++)
        if (p[i] != prefix[i])
            return 0;
    return 1;
}

static int hex_digit(char c)
{
    if (c >= '0' && c <= '9')
        return c - '0';
    if (c >= 'a' && c <= 'f')
        return c - 'a' + 10;
    return -1;
}

/* Parses "<start>-<end> perms offset dev inode   path" and returns 1 for a
 * mapping of a device file. */
static int parse_line(const char *l, size_t n, uintptr_t *start, uintptr_t *end)
{
    size_t i = 0;
    uintptr_t s = 0, e = 0;
    int d;

    while (i < n && (d = hex_digit(l[i])) >= 0)
        s = s * 16 + (uintptr_t)d, i++;
    if (i >= n || l[i] != '-')
        return 0;
    i++;
    while (i < n && (d = hex_digit(l[i])) >= 0)
        e = e * 16 + (uintptr_t)d, i++;
    /* Skip perms, offset, dev and inode. */
    for (int field = 0; field < 4; field++) {
        while (i < n && l[i] == ' ')
            i++;
        while (i < n && l[i] != ' ')
            i++;
    }
    while (i < n && l[i] == ' ')
        i++;
    if (!is_device_path(l + i, n - i) || e <= s)
        return 0;
    *start = s;
    *end = e;
    return 1;
}

/* One pass over /proc/self/maps: unmaps up to MAX_RANGES device mappings.
 * Returns 1 if there may be more, 0 when done (or on error). */
static int unmap_pass(void)
{
    static char buf[16384];
    static uintptr_t starts[MAX_RANGES], ends[MAX_RANGES];
    size_t count = 0, have = 0;
    int full = 0;

    int fd = open("/proc/self/maps", O_RDONLY | O_CLOEXEC);
    if (fd < 0)
        return 0;
    for (;;) {
        ssize_t r = read(fd, buf + have, sizeof(buf) - have);
        if (r < 0)
            break;
        have += (size_t)r;
        size_t pos = 0;
        for (;;) {
            size_t eol = pos;
            while (eol < have && buf[eol] != '\n')
                eol++;
            if (eol == have && r != 0)
                break; /* incomplete line, read more */
            if (eol > pos) {
                uintptr_t s, e;
                if (parse_line(buf + pos, eol - pos, &s, &e)) {
                    if (count == MAX_RANGES) {
                        full = 1;
                        break;
                    }
                    starts[count] = s;
                    ends[count] = e;
                    count++;
                }
            }
            pos = eol + 1;
            if (pos >= have)
                break;
        }
        if (full || r == 0)
            break;
        if (pos < have) {
            /* Keep the incomplete line; a line longer than the buffer
             * (path names are limited to PATH_MAX) is dropped. */
            size_t rest = have - pos;
            if (pos == 0) {
                have = 0;
                continue;
            }
            for (size_t k = 0; k < rest; k++)
                buf[k] = buf[pos + k];
            have = rest;
        } else {
            have = 0;
        }
    }
    close(fd);
    for (size_t k = 0; k < count; k++)
        munmap((void *)starts[k], ends[k] - starts[k]);
    return full;
}

struct linux_dirent64 {
    uint64_t d_ino;
    int64_t d_off;
    unsigned short d_reclen;
    unsigned char d_type;
    char d_name[];
};

static void close_device_fds(void)
{
    static char dents[4096];
    static char link[64];
    int dir = open("/proc/self/fd", O_RDONLY | O_DIRECTORY | O_CLOEXEC);
    if (dir < 0)
        return;
    for (;;) {
        long n = syscall(SYS_getdents64, dir, dents, sizeof(dents));
        if (n <= 0)
            break;
        for (long off = 0; off < n;) {
            struct linux_dirent64 *d = (struct linux_dirent64 *)(dents + off);
            off += d->d_reclen;
            int fd = 0, digits = 0;
            for (const char *c = d->d_name; *c; c++) {
                if (*c < '0' || *c > '9') {
                    digits = 0;
                    break;
                }
                fd = fd * 10 + (*c - '0');
                digits++;
            }
            if (!digits || fd == dir)
                continue;
            ssize_t len = readlinkat(dir, d->d_name, link, sizeof(link));
            if (len > 0 && is_device_path(link, (size_t)len))
                close(fd);
        }
    }
    close(dir);
}

static void drop_device_state(void)
{
    for (int pass = 0; pass < MAX_PASSES && unmap_pass(); pass++)
        ;
    close_device_fds();
}

__attribute__((constructor)) static void forkguard_init(void)
{
    const char *test = getenv("PUZUZ_FORKGUARD_TEST_PREFIX");
    size_t n = test ? strlen(test) : 0;
    if (n > 0 && n < sizeof(prefix)) {
        memcpy(prefix, test, n + 1);
        prefix_len = n;
    }
    real_fork = (pid_t(*)(void))dlsym(RTLD_NEXT, "fork");
}

EXPORT pid_t fork(void)
{
    pid_t (*f)(void) = real_fork;
    if (!f) /* fork() during another library's constructor */
        f = (pid_t(*)(void))dlsym(RTLD_NEXT, "fork");
    if (!f) {
        errno = ENOSYS;
        return -1;
    }
    pid_t pid = f();
    if (pid == 0)
        drop_device_state();
    return pid;
}
