#include <stdio.h>
#include <unistd.h>
#include <signal.h>
#include <sys/resource.h>
#include <bpf/libbpf.h>
#include "dl_miss_count.skel.h" // 引入刚才生成的脚手架

static volatile bool exiting = false;

static void sig_handler(int sig)
{
    exiting = true;
}

int main(int argc, char **argv)
{
    struct dl_miss_count_bpf *skel; // 自动生成的结构体名通常是文件名 + _bpf
    int err;

    // 1. 设置信号处理，方便 Ctrl+C 退出
    signal(SIGINT, sig_handler);
    signal(SIGTERM, sig_handler);

    // 2. 打开 BPF 程序
    skel = dl_miss_count_bpf__open();
    if (!skel) {
        fprintf(stderr, "Failed to open and load BPF skeleton\n");
        return 1;
    }

    // 3. 加载到内核
    err = dl_miss_count_bpf__load(skel);
    if (err) {
        fprintf(stderr, "Failed to load and verify BPF skeleton\n");
        goto cleanup;
    }

    // 4. 挂载 (Attach) Tracepoint
    err = dl_miss_count_bpf__attach(skel);
    if (err) {
        fprintf(stderr, "Failed to attach BPF skeleton\n");
        goto cleanup;
    }

    printf("Successfully started! Press Ctrl+C to stop.\n");

    // 5. 循环读取统计数据
    // 这里演示每秒读取一次 stats map
    while (!exiting) {
        sleep(1);
        // 如果你需要读取 Map 数据，可以使用 bpf_map_lookup_elem
        // 或者遍历 skel->maps.stats
        // 这里的逻辑可以根据你的需求扩展
        printf("."); 
        fflush(stdout);
    }

cleanup:
    // 6. 清理并卸载
    dl_miss_count_bpf__destroy(skel);
    return -err;
}
