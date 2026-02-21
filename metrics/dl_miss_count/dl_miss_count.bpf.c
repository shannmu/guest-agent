#include "vmlinux.h"
#include <bpf/bpf_helpers.h>


/* 配置 */
#define EPOCH_NS 1000000000ULL // 1秒一个 Epoch

/* * 唯一标识一个 Deadline 周期: PID + Deadline时间戳
 * 只要这两者没变，说明还是同一个 Miss 事件在持续
 */
struct miss_key_t {
    u32 pid;
    u64 deadline_ts;
};

struct epoch_stat_t {
    u64 total_misses;
    s64 max_lateness;
};

/* * Map 1: 去重过滤器 (LRU Hash)
 * Key: miss_key_t (PID + Deadline)
 * Value: 任意 (例如 1)
 * 作用: 如果 Key 存在，说明这个 Miss 已经在这个周期被统计过了，直接跳过。
 * LRU 会自动淘汰很久以前的记录，防止内存溢出。
 */
struct {
    __uint(type, BPF_MAP_TYPE_LRU_HASH);
    __uint(max_entries, 10240); // 根据系统任务量调整
    __type(key, struct miss_key_t);
    __type(value, u8);
} seen_misses SEC(".maps");

/* * Map 2: 统计结果 (Per-CPU Hash)
 * Key: Epoch ID (时间窗口)
 * Value: 统计数据
 */
struct {
    __uint(type, BPF_MAP_TYPE_PERCPU_HASH);
    __uint(max_entries, 1024);
    __type(key, u64); 
    __type(value, struct epoch_stat_t);
} stats SEC(".maps");

SEC("tracepoint/sched/sched_deadline_miss")
int handle_deadline_miss(struct trace_event_raw_sched_deadline_miss *ctx)
{
    struct miss_key_t key = {};
    key.pid = ctx->pid;
    key.deadline_ts = ctx->deadline;

    // 1. 去重检查
    u8 *seen = bpf_map_lookup_elem(&seen_misses, &key);
    if (seen) {
        // 这个 miss 已经被记录过了，直接返回，忽略本次事件
        return 0;
    }

    // 2. 标记为已见 (插入去重 Map)
    u8 val = 1;
    bpf_map_update_elem(&seen_misses, &key, &val, BPF_ANY);

    // 3. 计算 Epoch 并更新统计
    u64 now = bpf_ktime_get_ns();
    u64 epoch = now / EPOCH_NS;
    
    struct epoch_stat_t *stat, zero_stat = {};
    s64 lateness = ctx->lateness;

    stat = bpf_map_lookup_elem(&stats, &epoch);
    if (!stat) {
        bpf_map_update_elem(&stats, &epoch, &zero_stat, BPF_NOEXIST);
        stat = bpf_map_lookup_elem(&stats, &epoch);
        if (!stat) return 0;
    }

    stat->total_misses++;
    if (lateness > stat->max_lateness) {
        stat->max_lateness = lateness;
    }

    return 0;
}

char LICENSE[] SEC("license") = "GPL";
