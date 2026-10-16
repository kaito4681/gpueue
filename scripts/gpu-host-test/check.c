/*
 * GPU counter for scripts/gpu-host-test.sh: a CUDA driver-API program whose
 * state lives in GPU memory, so a checkpoint/restore that loses or resets it
 * is detected.
 *
 * Every second it adds i+1 on the GPU to each of 4096 ints, copies them back
 * and checks value[i] == step*(i+1). After each verified step it prints
 * `VERIFIED step=N`, rewrites DIR/progress ("step pid last_value") and appends
 * the step to DIR/history. It exits 0 with `PASS` once DIR/stop exists, 3 on
 * a mismatch, 2 on a CUDA error and 5 when MAX_STEPS pass without a stop file
 * (so a forgotten job cannot run for ever).
 *
 * It needs no CUDA toolkit: libcuda is loaded with dlopen and the kernel is
 * inline PTX that the driver JIT-compiles for whatever GPU it runs on. Build:
 *   cc -O2 -o check check.c -ldl
 * Usage: check DIR [MAX_STEPS]   (default 900)
 */
#include <dlfcn.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>

#define N 4096

typedef int CUresult;
typedef int CUdevice;
typedef struct CUctx_st *CUcontext;
typedef struct CUmod_st *CUmodule;
typedef struct CUfunc_st *CUfunction;
typedef struct CUstream_st *CUstream;
typedef uint64_t CUdeviceptr;

/* PTX ISA 6.0 / sm_50 is the oldest target current drivers still JIT-compile;
 * it only uses integer instructions every generation since Maxwell has, and
 * the driver compiles it for the actual GPU (Ampere, Hopper, Blackwell...). */
#ifndef PTX_HEADER
#define PTX_HEADER ".version 6.0\n.target sm_50\n.address_size 64\n"
#endif
static const char ptx[] = PTX_HEADER
    ".visible .entry advance(.param .u64 ptr) {\n"
    ".reg .u32 r<8>; .reg .u64 d<4>;\n"
    "ld.param.u64 d0, [ptr]; mov.u32 r0, %ctaid.x; mov.u32 r1, %ntid.x; mov.u32 r2, %tid.x;\n"
    "mad.lo.u32 r3, r0, r1, r2; mul.wide.u32 d1, r3, 4; add.u64 d2, d0, d1;\n"
    "ld.global.u32 r4, [d2]; add.u32 r5, r3, 1; add.u32 r6, r4, r5; st.global.u32 [d2], r6; ret; }\n";

static struct {
    CUresult (*Init)(unsigned);
    CUresult (*DeviceGet)(CUdevice *, int);
    CUresult (*DeviceGetName)(char *, int, CUdevice);
    CUresult (*DeviceGetAttribute)(int *, int, CUdevice);
    CUresult (*PrimaryCtxRetain)(CUcontext *, CUdevice);
    CUresult (*CtxSetCurrent)(CUcontext);
    CUresult (*ModuleLoadData)(CUmodule *, const void *);
    CUresult (*ModuleGetFunction)(CUfunction *, CUmodule, const char *);
    CUresult (*MemAlloc)(CUdeviceptr *, size_t);
    CUresult (*MemsetD32)(CUdeviceptr, unsigned, size_t);
    CUresult (*MemcpyDtoH)(void *, CUdeviceptr, size_t);
    CUresult (*MemFree)(CUdeviceptr);
    CUresult (*LaunchKernel)(CUfunction, unsigned, unsigned, unsigned, unsigned, unsigned,
                             unsigned, unsigned, CUstream, void **, void **);
    CUresult (*CtxSynchronize)(void);
    CUresult (*GetErrorString)(CUresult, const char **);
} cu;

#define GPU(call)                                                              \
    do {                                                                       \
        CUresult e_ = (call);                                                  \
        if (e_ != 0) {                                                         \
            const char *msg_ = "unknown error";                                \
            if (cu.GetErrorString) cu.GetErrorString(e_, &msg_);               \
            fprintf(stderr, "CUDA ERROR %s: %d %s\n", #call, e_, msg_);        \
            return 2;                                                          \
        }                                                                      \
    } while (0)

static int load(void) {
    void *lib = dlopen("libcuda.so.1", RTLD_NOW);
    if (!lib) lib = dlopen("libcuda.so", RTLD_NOW);
    if (!lib) {
        fprintf(stderr, "cannot load libcuda: %s\n", dlerror());
        return -1;
    }
#define SYM(field, name)                                                       \
    if (!(*(void **)&cu.field = dlsym(lib, name))) {                           \
        fprintf(stderr, "libcuda has no %s\n", name);                          \
        return -1;                                                             \
    }
    SYM(Init, "cuInit");
    SYM(DeviceGet, "cuDeviceGet");
    SYM(DeviceGetName, "cuDeviceGetName");
    SYM(DeviceGetAttribute, "cuDeviceGetAttribute");
    SYM(PrimaryCtxRetain, "cuDevicePrimaryCtxRetain");
    SYM(CtxSetCurrent, "cuCtxSetCurrent");
    SYM(ModuleLoadData, "cuModuleLoadData");
    SYM(ModuleGetFunction, "cuModuleGetFunction");
    SYM(MemAlloc, "cuMemAlloc_v2");
    SYM(MemsetD32, "cuMemsetD32_v2");
    SYM(MemcpyDtoH, "cuMemcpyDtoH_v2");
    SYM(MemFree, "cuMemFree_v2");
    SYM(LaunchKernel, "cuLaunchKernel");
    SYM(CtxSynchronize, "cuCtxSynchronize");
    SYM(GetErrorString, "cuGetErrorString");
#undef SYM
    return 0;
}

int main(int argc, char **argv) {
    if (argc < 2 || argc > 3) {
        fprintf(stderr, "usage: %s DIR [MAX_STEPS]\n", argv[0]);
        return 2;
    }
    long max_steps = argc == 3 ? strtol(argv[2], NULL, 10) : 900;
    if (load() != 0) return 2;

    CUdevice dev;
    CUcontext ctx;
    CUmodule module;
    CUfunction kernel;
    CUdeviceptr device;
    static int host[N];
    char name[256] = "";
    int major = 0, minor = 0;
    GPU(cu.Init(0));
    GPU(cu.DeviceGet(&dev, 0));
    GPU(cu.DeviceGetName(name, sizeof(name), dev));
    GPU(cu.DeviceGetAttribute(&major, 75, dev)); /* COMPUTE_CAPABILITY_MAJOR */
    GPU(cu.DeviceGetAttribute(&minor, 76, dev)); /* COMPUTE_CAPABILITY_MINOR */
    GPU(cu.PrimaryCtxRetain(&ctx, dev));
    GPU(cu.CtxSetCurrent(ctx));
    GPU(cu.ModuleLoadData(&module, ptx));
    GPU(cu.ModuleGetFunction(&kernel, module, "advance"));
    GPU(cu.MemAlloc(&device, sizeof(host)));
    GPU(cu.MemsetD32(device, 0, N));

    char progress[4096], history[4096], stop[4096];
    snprintf(progress, sizeof(progress), "%s/progress", argv[1]);
    snprintf(history, sizeof(history), "%s/history", argv[1]);
    snprintf(stop, sizeof(stop), "%s/stop", argv[1]);
    const char *visible = getenv("CUDA_VISIBLE_DEVICES");
    printf("START GPU pid=%d device=\"%s\" sm_%d%d CUDA_VISIBLE_DEVICES=%s\n", getpid(), name,
           major, minor, visible ? visible : "(unset)");
    fflush(stdout);

    void *args[] = {&device};
    long step = 0;
    while (step < max_steps) {
        GPU(cu.LaunchKernel(kernel, N / 256, 1, 1, 256, 1, 1, 0, NULL, args, NULL));
        GPU(cu.CtxSynchronize());
        GPU(cu.MemcpyDtoH(host, device, sizeof(host)));
        ++step;
        for (int i = 0; i < N; ++i) {
            if ((long)host[i] != step * (i + 1)) {
                fprintf(stderr, "FAIL step=%ld index=%d got=%d expected=%ld\n", step, i, host[i],
                        step * (i + 1));
                return 3;
            }
        }
        FILE *f = fopen(progress, "w");
        if (!f) return 4;
        fprintf(f, "%ld %d %d\n", step, getpid(), host[N - 1]);
        fclose(f);
        f = fopen(history, "a");
        if (!f) return 4;
        fprintf(f, "%ld\n", step);
        fclose(f);
        printf("VERIFIED step=%ld pid=%d last=%d\n", step, getpid(), host[N - 1]);
        fflush(stdout);
        if (access(stop, F_OK) == 0) {
            GPU(cu.MemFree(device));
            printf("PASS steps=%ld pid=%d (all %d values verified at every step)\n", step,
                   getpid(), N);
            return 0;
        }
        sleep(1);
    }
    fprintf(stderr, "FAIL no stop file after %ld steps\n", step);
    return 5;
}
