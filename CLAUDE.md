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

```
