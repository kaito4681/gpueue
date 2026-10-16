/*
 * CPU counter for scripts/gpu-host-test.sh: the same protocol as check.c with
 * its state in process memory. Every second it adds i+1 to each of 4096 ints,
 * checks value[i] == step*(i+1), prints `VERIFIED step=N`, rewrites
 * DIR/progress and appends to DIR/history. Exits 0 with `PASS` once DIR/stop
 * exists, 3 on a mismatch and 5 after MAX_STEPS without a stop file.
 *   cc -O2 -o cpu cpu.c
 * Usage: cpu DIR [MAX_STEPS]   (default 900)
 */
#include <stdio.h>
#include <stdlib.h>
#include <unistd.h>

#define N 4096

int main(int argc, char **argv) {
    if (argc < 2 || argc > 3) {
        fprintf(stderr, "usage: %s DIR [MAX_STEPS]\n", argv[0]);
        return 2;
    }
    long max_steps = argc == 3 ? strtol(argv[2], NULL, 10) : 900;
    long *state = calloc(N, sizeof(long));
    if (!state) return 2;
    char progress[4096], history[4096], stop[4096];
    snprintf(progress, sizeof(progress), "%s/progress", argv[1]);
    snprintf(history, sizeof(history), "%s/history", argv[1]);
    snprintf(stop, sizeof(stop), "%s/stop", argv[1]);
    printf("START CPU pid=%d\n", getpid());
    fflush(stdout);
    for (long step = 1; step <= max_steps; ++step) {
        for (int i = 0; i < N; ++i) {
            state[i] += i + 1;
            if (state[i] != step * (i + 1)) {
                fprintf(stderr, "FAIL step=%ld index=%d got=%ld\n", step, i, state[i]);
                return 3;
            }
        }
        FILE *f = fopen(progress, "w");
        if (!f) return 4;
        fprintf(f, "%ld %d %ld\n", step, getpid(), state[N - 1]);
        fclose(f);
        f = fopen(history, "a");
        if (!f) return 4;
        fprintf(f, "%ld\n", step);
        fclose(f);
        printf("VERIFIED step=%ld pid=%d last=%ld\n", step, getpid(), state[N - 1]);
        fflush(stdout);
        if (access(stop, F_OK) == 0) {
            printf("PASS steps=%ld pid=%d\n", step, getpid());
            free(state);
            return 0;
        }
        sleep(1);
    }
    fprintf(stderr, "FAIL no stop file after %ld steps\n", max_steps);
    free(state);
    return 5;
}
