// Minimal CO-RE kernel struct definitions (`preserve_access_index`) so BPF programs compile
// without a full `vmlinux.h` and relocate cleanly against the running kernel's BTF.
#pragma once

#include "records.h"

#define __kernel_preserve __attribute__((preserve_access_index))

struct pt_regs {
	unsigned long r15;
	unsigned long r14;
	unsigned long r13;
	unsigned long r12;
	unsigned long bp;
	unsigned long bx;
	unsigned long r11;
	unsigned long r10;
	unsigned long r9;
	unsigned long r8;
	unsigned long ax;
	unsigned long cx;
	unsigned long dx;
	unsigned long si;
	unsigned long di;
	unsigned long orig_ax;
	unsigned long ip;
	unsigned long sp;
} __kernel_preserve;

struct thread_info {
	u32 status;
} __kernel_preserve;

typedef struct {
	int counter;
} atomic_t;

struct signal_struct {
	atomic_t live;
	int nr_threads;
	unsigned int flags;
	int group_exit_code;
} __kernel_preserve;

struct super_block {
	unsigned long s_magic;
} __kernel_preserve;

struct inode {
	unsigned short i_mode;
	unsigned int i_rdev;
	unsigned long i_ino;
	struct super_block *i_sb;
} __kernel_preserve;

struct qstr {
	const unsigned char *name;
} __kernel_preserve;

struct dentry {
	struct qstr d_name;
} __kernel_preserve;

struct path {
	struct dentry *dentry;
} __kernel_preserve;

struct file {
	struct path f_path;
	struct inode *f_inode;
} __kernel_preserve;

struct fdtable {
	unsigned int max_fds;
	struct file **fd;
	unsigned long *close_on_exec;
} __kernel_preserve;

struct files_struct {
	struct fdtable *fdt;
} __kernel_preserve;

struct ns_common {
	unsigned int inum;
} __kernel_preserve;

struct pid_namespace {
	struct ns_common ns;
} __kernel_preserve;

struct upid {
	int nr;
	struct pid_namespace *ns;
} __kernel_preserve;


struct pid {
	unsigned int level;
	struct upid numbers[1];
} __kernel_preserve;

struct task_struct {
	struct thread_info thread_info;
	int pid;
	int tgid;
	int exit_code;
	struct signal_struct *signal;
	struct files_struct *files;
	struct pid *thread_pid;
	struct task_struct *group_leader;
} __kernel_preserve;

struct linux_binprm {
	const char *filename;
} __kernel_preserve;

struct kernel_siginfo {
	int si_signo;
} __kernel_preserve;

struct k_sigaction;

struct seq_file;

struct bpf_iter_meta {
	struct seq_file *seq;
} __kernel_preserve;

struct bpf_iter__task {
	struct bpf_iter_meta *meta;
	struct task_struct *task;
} __kernel_preserve;
