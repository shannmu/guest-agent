# Design Doc
本项目是一个运行在虚拟机内用户态的守护进程Agent, 通过共享内存和宿主机进行通信
向宿主机提供各个vCPU的QoS压力

## Spec Design
- 初始化
  - 共享内存: 共享内存通过kernel cmdline传入Guest操作系统, Guest操作系统内有内核模块pvsched_guest用于处理共享内存，并向用户态提供服务
    - kernel cmdline
      这是kernel的实现部分，这里不具体展开
    - pvsched_guest提供/dev/pvsched_guest的mmap操作，用于将该共享内存暴露给用户态操作
  - Agent进程
    - 首先提供守护进程的基本能力，即保活能力，panic后迅速拉起
    - 初始化时open + mmap /dev/pvsched_guest, 得到共享内存指针, 并对共享内存中的Guest可写区域进行初始化

- Qos反馈能力
  - 需要使用用户态高精度定时器，触发时间为参数
    用于触发QoS收集与写入共享内存
  - Qos收集，通过ebpf, /proc/文件系统等多种方式进行，定义一个统一的trait，具体的Qos则通过实现trait

## FIXME List
- [x] 现在的QoS反馈能力的高精度定时器是通过loop实现的，而不是真正的定时器触发，需要修复该问题
- [x] 如果你用 it_interval=interval，timerfd 的周期触发本身很稳，但用户态采集的耗时会造成“采样时刻抖动”。
更关键的是很多采集希望对齐到整秒/整 100ms（方便聚合、画图、对齐其他数据源）。
你可以用 timerfd_settime 的 TFD_TIMER_ABSTIME + 每次设定下一个 deadline（或初始化一次对齐到边界的 it_value）。
典型做法（思路）：
取当前 CLOCK_MONOTONIC（或 CLOCK_REALTIME，看你要不要受系统时间调整影响）
计算下一个“对齐边界”时刻（比如下一秒）
TFD_TIMER_ABSTIME 设一次性触发
每次触发后，deadline += interval，再设下一次
这样即使采集偶尔慢了一点，也不会长期漂移。
- [x] PSI QoS实现
```
# 如何计算 QoS（基于 cgroup v2 的 CPU PSI）
1. 读取 GNU Radio所在的 cgroup文件，查看其被固定在哪个CPU

2. 读取 GNU Radio 所在 cgroup 的文件：

   /sys/fs/cgroup/<gnuradio_cgroup>/cpu.pressure

3. 从文件第一行（以 `some` 开头）中取出 `total` 的数值：

   some ... total=<total_us>

   其中 `total_us` 是累计的 CPU stall 时间，单位为微秒（µs）。

4. 设定采样周期（例如 50ms 或 100ms）。每次采样时同时记录：
   - 当前时间戳 T（用单调时钟，单位 µs）
   - 当前 total 值 P（单位 µs）

5. 用相邻两次采样计算：

   Δt     = T2 - T1  
   Δstall = P2 - P1

6. 计算 stall 比例并得到 QoS：

  stall_ratio = Δstall / Δt  
  QoS = min(1, stall_ratio)

QoS 的取值范围是 0~1；越接近 0 表示任务越少因 CPU 竞争而被阻塞。
```
- [x] VcpuStat中, 只需要一个pressure变量, 不需要deadline_miss_count, deadline_lateness_ns等qos细节
  pressure为一个0-1的无量纲量
- [x] 我提供了pvsched.h头文件，这是与host通信的共享内存ABI定义文件，主要是pvsched_shared_mem的声明
  你需要修复对应rust代码中的对共享内存的改动相关的函数，rust代码中只能读共享内存和写qos_pressure(且是使用原子操作的方式) 
- [x] 在metrics目录下，实现一个python代码，每一秒触发一次（不需要很精确），收集/sys/fs/cgroup/gnuradio.slice和/sys/fs/cgroup/yamcs.slice的psi pressure, 以及所有cpu的runqueue p95 wait time的均值
  将该数据保存在csv文件中，每次启动该python代码，都是append数据的形式。需要打时间戳
- [x] 请修复rust代码中的psi pressure相关代码
`````
```
1. GNU Radiod所在的cgroup文件修改为: /sys/fs/cgroup/gnuradio.slice/cpu.pressure

2. 在每次timer arm时，添加对yamcs任务的QoS收集，一样是psi pressure, 只不过Yamcs所在的cgroup文件为/sys/fs/cgroup/yamcs.slice/cpu.pressure

3. 注意，所在的cpu路径也随着cgroup路径改变需要修改
```
