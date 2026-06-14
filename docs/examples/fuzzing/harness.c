// harness.c — the fuzzer brain + coverage callback. The userspace TWIN of
// ignition's M0/M1 loop. fork() stands in for VM snapshot/reset.
// Compiled WITHOUT -fsanitize-coverage (so the callback isn't instrumented).
//
//   ignition concept            | this demo
//   --------------------------- | --------------------------------
//   snapshot point / reset()    | fork() per input (CoW child)
//   immutable base              | the parent process (never mutated)
//   hv_vm_map'd shared window    | MAP_SHARED|MAP_ANONYMOUS regions
//   SanCov counters in window   | cov[] hashed from trace-pc callback
//   DONE / CRASH doorbell       | normal exit vs. signal (waitpid)
//   libAFL feedback/corpus      | the corpus[] + virgin-map below
#include <stdio.h>
#include <stdlib.h>
#include <stdint.h>
#include <string.h>
#include <unistd.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <time.h>
#include <fcntl.h>

#define MAPSZ   (1<<16)     // 64 KiB coverage map (AFL-sized)
#define MAXIN   4096
#define CORPMAX 4096

void parse(const uint8_t *d, unsigned long n);   // the target

static uint8_t *cov;        // shared: child writes, parent reads
static uint8_t *inbuf;      // shared: parent writes input, child reads
static uint32_t *inlen;     // shared: input length

// --- coverage callback (NOT instrumented; lives in this TU) ---
void __sanitizer_cov_trace_pc(void) {
    if (!cov) return;  // fires during global init before main mmaps the map
    uintptr_t pc = (uintptr_t)__builtin_return_address(0);
    cov[(pc >> 4) & (MAPSZ - 1)]++;     // hash edge location into the map
}

// --- corpus ---
static uint8_t corpus[CORPMAX][MAXIN];
static uint32_t corplen[CORPMAX];
static int ncorp = 0;
static uint8_t virgin[MAPSZ];           // accumulated coverage (parent-only)

static uint64_t rng_s = 0x1234567;
static uint64_t rnd(void){ rng_s ^= rng_s<<13; rng_s ^= rng_s>>7; rng_s ^= rng_s<<17; return rng_s; }

static uint32_t mutate(uint8_t *out) {
    uint32_t len; 
    if (ncorp == 0) { len = 1 + rnd()%8; for(uint32_t i=0;i<len;i++) out[i]=rnd(); return len; }
    int src = rnd()%ncorp; len = corplen[src]; memcpy(out, corpus[src], len);
    int rounds = 1 + rnd()%6;
    for (int r=0;r<rounds;r++){
        switch(rnd()%5){
        case 0: if(len) out[rnd()%len] ^= 1<<(rnd()%8); break;          // bit flip
        case 1: if(len) out[rnd()%len] = rnd(); break;                  // byte set
        case 2: if(len<MAXIN){ uint32_t p=rnd()%(len+1); memmove(out+p+1,out+p,len-p); out[p]=rnd(); len++; } break; // insert
        case 3: if(len>1){ uint32_t p=rnd()%len; memmove(out+p,out+p+1,len-p-1); len--; } break;                     // delete
        case 4: if(len>=4){ uint32_t p=rnd()%(len-3); out[p]='F';out[p+1]='U';out[p+2]='Z';out[p+3]=1; } break;      // dict token
        }
    }
    return len;
}

static int new_cov(void){            // did the child hit an edge we've never seen?
    int found=0;
    for(int i=0;i<MAPSZ;i++) if(cov[i] && !virgin[i]){ virgin[i]=1; found=1; }
    return found;
}

int main(int argc, char**argv){
    setvbuf(stdout,NULL,_IONBF,0);
    long budget = argc>1 ? atol(argv[1]) : 2000000;
    cov   = mmap(0,MAPSZ, PROT_READ|PROT_WRITE, MAP_SHARED|MAP_ANONYMOUS,-1,0);
    inbuf = mmap(0,MAXIN, PROT_READ|PROT_WRITE, MAP_SHARED|MAP_ANONYMOUS,-1,0);
    inlen = mmap(0,4096,  PROT_READ|PROT_WRITE, MAP_SHARED|MAP_ANONYMOUS,-1,0);

    { uint8_t seed[] = {'F','U','Z',1, 'C',16,0, 1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20};
      // valid: C chunk len=16 == buf size (no overflow), with 20 data bytes present.
      // bumping the len byte to 17..20 -> in-bounds but >16 -> heap overflow.
      memcpy(corpus[0],seed,sizeof seed); corplen[0]=sizeof seed; ncorp=1;
      printf("seeded corpus with 1 boundary input (%zu bytes)\n",sizeof seed); }
    struct timespec t0; clock_gettime(CLOCK_MONOTONIC,&t0);
    long execs=0; uint8_t scratch[MAXIN];

    for (long it=0; it<budget; it++){
        uint32_t len = mutate(scratch); if(len>MAXIN) len=MAXIN;
        memcpy(inbuf,scratch,len); *inlen=len;     // inject into shared window
        memset(cov,0,MAPSZ);                       // zero coverage (host-managed page)

        pid_t pid = fork();                        // <-- the "reset": fresh CoW child
        if (pid==0){                               // child = the guest at the snapshot
            int dn=open("/dev/null",1); dup2(dn,2);// silence ASan during campaign
            parse(inbuf,*inlen);                   // run the target on injected input
            _exit(0);                              // "DONE doorbell"
        }
        int st; waitpid(pid,&st,0); execs++;
        if(execs%50000==0) printf("  ...%ld execs, corpus=%d\n",execs,ncorp);

        if (WIFSIGNALED(st)){                      // "CRASH doorbell" (ASan -> SIGABRT)
            struct timespec t1; clock_gettime(CLOCK_MONOTONIC,&t1);
            double secs=(t1.tv_sec-t0.tv_sec)+(t1.tv_nsec-t0.tv_nsec)/1e9;
            printf("\n*** CRASH after %ld execs (%.2fs, %.0f execs/sec), %d corpus entries\n",
                   execs,secs,execs/secs,ncorp);
            printf("    crashing input (%u bytes): ",len);
            for(uint32_t i=0;i<len && i<24;i++) printf("%02x ",scratch[i]); printf("\n");
            FILE*f=fopen(argv[2]?argv[2]:"crash.bin","wb"); fwrite(scratch,1,len,f); fclose(f);
            return 0;
        }
        if (new_cov() && ncorp<CORPMAX){           // "interesting" -> keep in corpus
            memcpy(corpus[ncorp],scratch,len); corplen[ncorp]=len; ncorp++;
            if(ncorp%4==0) printf("  corpus=%-4d execs=%-9ld\n",ncorp,execs);
        }
    }
    printf("budget exhausted, no crash. corpus=%d execs=%ld\n",ncorp,execs);
    return 1;
}
