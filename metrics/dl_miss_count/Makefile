# Makefile
OUTPUT := .
CLANG ?= clang
BPFTOOL ?= bpftool
ARCH ?= $(shell uname -m | sed 's/x86_64/x86/' | sed 's/aarch64/arm64/')

# 目标文件
APP := dl_miss_count

all: $(APP)

# 1. 编译 BPF 对象
dl_miss_count.bpf.o: dl_miss_count.bpf.c vmlinux.h
	$(CLANG) -g -O2 -target bpf -D__TARGET_ARCH_$(ARCH) -I. -c dl_miss_count.bpf.c -o $@

# 2. 生成 Skeleton
dl_miss_count.skel.h: dl_miss_count.bpf.o
	$(BPFTOOL) gen skeleton $< > $@

# 3. 编译用户态程序
$(APP): main.c dl_miss_count.skel.h
	$(CLANG) -g -O2 -Wall -I. main.c -lbpf -lelf -o $@

clean:
	rm -f *.o *.skel.h $(APP)
