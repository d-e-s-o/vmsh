/*
 * Minimal init process for vmsh.
 *
 * This binary runs as PID 1 inside the guest VM. It mounts essential
 * filesystems, brings up the loopback interface, sets the hostname,
 * then forks and exec's the user-specified command. The parent (PID 1)
 * waits for the child and reports the exit code back to the VMM via
 * a virtiofs ioctl.
 */

#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <net/if.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mount.h>
#include <sys/reboot.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/statfs.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

/* Ioctl number used by libkrun's virtiofs to communicate exit codes. */
#define KRUN_EXIT_CODE_IOCTL 0x7602

/* virtiofs magic number from statfs. */
#define VIRTIOFS_MAGIC 0x65735546

static void mkdir_p(const char *path) {
  /* Ignore errors (EEXIST is expected). */
  mkdir(path, 0755);
}

static int mount_or_err(const char *source, const char *target,
                        const char *fstype, unsigned long flags) {
  int ret = mount(source, target, fstype, flags, NULL);
  if (ret < 0) {
    /* EBUSY on /dev is OK (already mounted). */
    if (strcmp(target, "/dev") == 0 && errno == EBUSY)
      return 0;
    fprintf(stderr, "vmsh-init: mount(%s): %s\n", target, strerror(errno));
    return -1;
  }
  return 0;
}

static void mount_or_warn(const char *source, const char *target,
                          const char *fstype, unsigned long flags,
                          const char *label) {
  int ret = mount(source, target, fstype, flags, NULL);
  if (ret < 0) {
    fprintf(stderr, "vmsh-init: warning: mount %s: %s\n", label,
            strerror(errno));
  }
}

static int kernel_supports_fs(const char *fstype) {
  FILE *f = fopen("/proc/filesystems", "r");
  if (!f)
    return 0;

  char line[256];
  while (fgets(line, sizeof(line), f)) {
    /* Each line is either "nodev\t<fstype>\n" or "\t<fstype>\n". */
    char *name = strchr(line, '\t');
    if (!name)
      continue;
    name++; /* skip the tab */
    size_t len = strlen(name);
    if (len > 0 && name[len - 1] == '\n')
      name[len - 1] = '\0';
    if (strcmp(name, fstype) == 0) {
      fclose(f);
      return 1;
    }
  }
  fclose(f);
  return 0;
}

static int mount_filesystems(void) {
  /* Create level-1 directories. */
  mkdir_p("/dev");
  mkdir_p("/proc");
  mkdir_p("/sys");

  if (mount_or_err("devtmpfs", "/dev", "devtmpfs", MS_RELATIME) < 0)
    return -1;
  if (mount_or_err("proc", "/proc", "proc",
                   MS_NODEV | MS_NOEXEC | MS_NOSUID | MS_RELATIME) < 0)
    return -1;
  if (mount_or_err("sysfs", "/sys", "sysfs",
                   MS_NODEV | MS_NOEXEC | MS_NOSUID | MS_RELATIME) < 0)
    return -1;

  if (kernel_supports_fs("debugfs")) {
    mkdir_p("/sys/kernel/debug");
    mount_or_warn("debugfs", "/sys/kernel/debug", "debugfs",
                  MS_NODEV | MS_NOEXEC | MS_NOSUID | MS_RELATIME, "debugfs");
  }

  if (kernel_supports_fs("tracefs")) {
    mkdir_p("/sys/kernel/tracing");
    mount_or_warn("tracefs", "/sys/kernel/tracing", "tracefs",
                  MS_NODEV | MS_NOEXEC | MS_NOSUID | MS_RELATIME, "tracefs");
  }

  if (kernel_supports_fs("bpf")) {
    mkdir_p("/sys/fs/bpf");
    mount_or_warn("bpffs", "/sys/fs/bpf", "bpf",
                  MS_NODEV | MS_NOEXEC | MS_NOSUID | MS_RELATIME, "bpffs");
  }

  mkdir_p("/sys/fs/cgroup");
  mount_or_warn("cgroup2", "/sys/fs/cgroup", "cgroup2",
                MS_NODEV | MS_NOEXEC | MS_NOSUID | MS_RELATIME, "cgroup2");

  /* Create level-2 directories (after devtmpfs is mounted). */
  mkdir_p("/dev/pts");
  mkdir_p("/dev/shm");

  if (mount_or_err("devpts", "/dev/pts", "devpts",
                   MS_NOEXEC | MS_NOSUID | MS_RELATIME) < 0)
    return -1;
  if (mount_or_err("tmpfs", "/dev/shm", "tmpfs",
                   MS_NOEXEC | MS_NOSUID | MS_RELATIME) < 0)
    return -1;

  /* Symlink /dev/fd -> /proc/self/fd (may fail if exists). */
  symlink("/proc/self/fd", "/dev/fd");
  symlink("/proc/self/fd/0", "/dev/stdin");
  symlink("/proc/self/fd/1", "/dev/stdout");
  symlink("/proc/self/fd/2", "/dev/stderr");

  return 0;
}

static void bring_up_loopback(void) {
  int sockfd = socket(AF_INET, SOCK_DGRAM, 0);
  if (sockfd < 0)
    return;

  struct ifreq ifr;
  memset(&ifr, 0, sizeof(ifr));
  strcpy(ifr.ifr_name, "lo");
  ifr.ifr_flags |= IFF_UP;

  ioctl(sockfd, SIOCSIFFLAGS, &ifr);
  close(sockfd);
}

/* Find a named virtio console port.
 *
 * Scans /sys/class/virtio-ports/ for a port whose name matches
 * `target_name`. On success, writes the device path (e.g.
 * "/dev/vport0p1") to `dev_path` and returns 0. Returns -1 if the port
 * is not found after `max_attempts` polling iterations (1 ms apart).
 */
static int find_virtio_port(const char *target_name, char *dev_path,
                            size_t dev_path_size, int max_attempts) {
  const struct timespec delay = {.tv_sec = 0, .tv_nsec = 1000000}; /* 1 ms */
  DIR *dir = NULL;

  for (int attempt = 0; attempt < max_attempts; attempt++) {
    if (attempt > 0)
      nanosleep(&delay, NULL);

    if (!dir) {
      dir = opendir("/sys/class/virtio-ports");
      if (!dir)
        continue;
    } else {
      rewinddir(dir);
    }

    struct dirent *entry;
    while ((entry = readdir(dir)) != NULL) {
      if (entry->d_name[0] == '.')
        continue;

      char name_path[512];
      snprintf(name_path, sizeof(name_path), "/sys/class/virtio-ports/%s/name",
               entry->d_name);

      FILE *f = fopen(name_path, "r");
      if (!f)
        continue;

      char port_name[64];
      if (!fgets(port_name, sizeof(port_name), f)) {
        fclose(f);
        continue;
      }
      fclose(f);

      /* Trim trailing whitespace. */
      size_t len = strlen(port_name);
      while (len > 0 &&
             (port_name[len - 1] == '\n' || port_name[len - 1] == '\r'))
        port_name[--len] = '\0';

      if (strcmp(port_name, target_name) == 0) {
        snprintf(dev_path, dev_path_size, "/dev/%s", entry->d_name);
        closedir(dir);
        return 0;
      }
    }
  }
  if (dir)
    closedir(dir);
  return -1;
}

/* Load environment variables from a file written by the host.
 *
 * The kernel command line has a limited number of env var slots
 * (CONFIG_INIT_ENV_ARG_LIMIT, typically 32), so we pass bulk env vars
 * through a file on the virtiofs-shared filesystem instead.
 *
 * The file format is one KEY=VALUE per line (newline-delimited, no
 * quoting). Lines without '=' or empty lines are skipped.
 */
static void load_env_file(void) {
  const char *path = getenv("VMSH_ENV_FILE");
  if (!path)
    return;

  FILE *f = fopen(path, "r");
  if (!f)
    return;

  /* Remove the file now that we have it open. */
  unlink(path);

  char *line = NULL;
  size_t cap = 0;
  ssize_t len;
  while ((len = getline(&line, &cap, f)) > 0) {
    /* Strip trailing newline. */
    if (len > 0 && line[len - 1] == '\n')
      line[--len] = '\0';
    if (len == 0)
      continue;
    /* putenv needs a persistent copy; it does NOT copy the string. */
    char *entry = strdup(line);
    if (entry)
      putenv(entry);
  }
  free(line);
  fclose(f);
}

static void set_exit_code(int code) {
  struct statfs buf;

  int rc = statfs("/", &buf);
  if (rc != 0) {
    fprintf(stderr, "vmsh-init: warning: could not statfs /\n");
    return;
  }
  if ((unsigned long)buf.f_type != VIRTIOFS_MAGIC)
    return;

  int fd = open("/", O_RDONLY);
  if (fd < 0) {
    fprintf(stderr,
            "vmsh-init: warning: could not open / for exit code ioctl\n");
    return;
  }

  ioctl(fd, KRUN_EXIT_CODE_IOCTL, code);
  close(fd);
}

/* Redirect stdin/stdout/stderr to virtio console ports.
 *
 * VMSH_STDIN, VMSH_STDOUT, and VMSH_STDERR signal which file
 * descriptors have been redirected and need a corresponding virtio
 * console port. Each port is discovered and dup2'd onto the
 * corresponding file descriptor.
 */
static void setup_redirects(void) {
  struct redirect {
    const char *port_name;
    int target_fd;
    int flags;
    int done;
  };

  struct redirect redirects[3];
  int count = 0;

  if (getenv("VMSH_STDIN"))
    redirects[count++] = (struct redirect){"krun-stdin", STDIN_FILENO, O_RDONLY, 0};
  if (getenv("VMSH_STDOUT"))
    redirects[count++] = (struct redirect){"krun-stdout", STDOUT_FILENO, O_WRONLY, 0};
  if (getenv("VMSH_STDERR"))
    redirects[count++] = (struct redirect){"krun-stderr", STDERR_FILENO, O_WRONLY, 0};

  if (count == 0)
    return;

  /* Poll all pending ports concurrently, 1 ms apart, up to 500 ms. */
  const struct timespec delay = {.tv_sec = 0, .tv_nsec = 1000000};
  int remaining = count;

  for (int attempt = 0; attempt < 500 && remaining > 0; attempt++) {
    if (attempt > 0)
      nanosleep(&delay, NULL);

    for (int i = 0; i < count; i++) {
      if (redirects[i].done)
        continue;

      char dev_path[512];
      if (find_virtio_port(redirects[i].port_name, dev_path, sizeof(dev_path), 1) < 0)
        continue;

      int fd = open(dev_path, redirects[i].flags);
      if (fd >= 0) {
        dup2(fd, redirects[i].target_fd);
        if (fd != redirects[i].target_fd)
          close(fd);
      }
      redirects[i].done = 1;
      remaining--;
    }
  }
}

static void do_exec(const char *exec_path, char **exec_argv) {
  execvp(exec_path, exec_argv);

  /* If exec returns, it failed. */
  int err = errno;
  fprintf(stderr, "vmsh-init: couldn't execute '%s': %s\n", exec_path,
          strerror(err));
  int code = (err == ENOENT) ? 127 : 126;
  set_exit_code(code);
  _exit(code);
}

int main(int argc, char *argv[]) {
  /* Remove our own binary from the filesystem now that we are running. We do
   * that because on the happy path the host code won't have a chance, because
   * libkrun exits the process hard on VM exit. */
  unlink(argv[0]);

  /* Set up shared mount propagation on root. */
  mount_or_warn(NULL, "/", NULL, MS_REC | MS_SHARED, "shared propagation on /");

  /* Mount essential filesystems. */
  if (mount_filesystems() < 0) {
    fprintf(stderr, "vmsh-init: failed to mount filesystems\n");
    set_exit_code(125);
    return 125;
  }

  /* Ensure FDs 0, 1, 2 are valid. The kernel may fail to open /dev/console at
   * boot, leaving them closed. Fill any invalid slot with /dev/null so child
   * processes never inherit bad file descriptors. */
  for (int fd = 0; fd <= 2; fd++) {
    if (fcntl(fd, F_GETFD) < 0) {
      int nfd = open("/dev/null", O_RDWR);
      if (nfd >= 0 && nfd != fd) {
        dup2(nfd, fd);
        close(nfd);
      }
    }
  }

  /* Create new session and set controlling terminal. */
  setsid();
  ioctl(0, TIOCSCTTY, 1);

  /* Bring up loopback interface. */
  bring_up_loopback();

  /* Load host environment variables from the shared filesystem. */
  load_env_file();

  /* Set hostname. */
  const char *hostname = getenv("HOSTNAME");
  if (!hostname)
    hostname = "localhost";
  sethostname(hostname, strlen(hostname));

  /* Apply HOME and TERM from KRUN_ prefixed env vars. */
  const char *krun_home = getenv("KRUN_HOME");
  if (krun_home)
    setenv("HOME", krun_home, 1);
  const char *krun_term = getenv("KRUN_TERM");
  if (krun_term)
    setenv("TERM", krun_term, 1);

  /* Determine working directory. */
  const char *krun_workdir = getenv("KRUN_WORKDIR");
  if (krun_workdir)
    chdir(krun_workdir);

  /* Determine the command to run. */
  const char *krun_init = getenv("KRUN_INIT");
  if (!krun_init)
    krun_init = "/bin/sh";

  /*
   * Build argv: krun_init plus any remaining args.
   * argv[0] = krun_init, argv[1..] = our argv[1..], NULL-terminated.
   *
   * Filter out `tsi_hijack` and `tsi_hijack_unix` arguments. These are
   * kernel command line parameters for TSI (Transparent Socket
   * Impersonation). When the kernel lacks TSI patches it passes them
   * through to init as regular arguments.
   */
  int tsi_warning = 0;
  int exec_argc = 1;
  for (int i = 1; i < argc; i++) {
    if (strcmp(argv[i], "tsi_hijack") == 0 ||
        strcmp(argv[i], "tsi_hijack_unix") == 0) {
      tsi_warning = 1;
      continue;
    }
    exec_argc++;
  }
  char **exec_argv = malloc((exec_argc + 1) * sizeof(char *));
  if (!exec_argv) {
    fprintf(stderr, "vmsh-init: malloc failed\n");
    set_exit_code(125);
    return 125;
  }
  exec_argv[0] = (char *)krun_init;
  int j = 1;
  for (int i = 1; i < argc; i++) {
    if (strcmp(argv[i], "tsi_hijack") == 0 ||
        strcmp(argv[i], "tsi_hijack_unix") == 0)
      continue;
    exec_argv[j++] = argv[i];
  }
  exec_argv[j] = NULL;

  /* Check if we should run directly as PID 1 (no fork). */
  const char *init_pid1 = getenv("KRUN_INIT_PID1");
  if (init_pid1 && strcmp(init_pid1, "1") == 0) {
    setup_redirects();
    if (tsi_warning)
      fprintf(stderr,
              "vmsh-init: warning: kernel does not support TSI networking; "
              "use a TSI-patched kernel or omit --net argument to vmsh\n");
    do_exec(krun_init, exec_argv);
  }

  /* Fork: child exec's the command, parent waits and reports exit code. */
  pid_t child_pid = fork();
  if (child_pid < 0) {
    fprintf(stderr, "vmsh-init: fork failed\n");
    set_exit_code(125);
    return 125;
  }

  if (child_pid == 0) {
    /* Child process. */
    setup_redirects();
    if (tsi_warning)
      fprintf(stderr,
              "vmsh-init: warning: kernel does not support TSI networking; "
              "use a TSI-patched kernel or omit --net argument to vmsh\n");
    do_exec(krun_init, exec_argv);
  }

  /* Parent (PID 1): wait for the child. */
  int status = 0;
  for (;;) {
    pid_t ret = waitpid(-1, &status, 0);
    if (ret == child_pid || ret < 0)
      break;
  }

  int exit_code;
  if (WIFEXITED(status))
    exit_code = WEXITSTATUS(status);
  else if (WIFSIGNALED(status))
    exit_code = WTERMSIG(status) + 128;
  else
    exit_code = 125;

  set_exit_code(exit_code);

  /*
   * PID 1 must not exit (kernel panics). Power off the VM:
   * - use RB_AUTOBOOT which triggers a reboot
   * - that goes through the reboot=k path (i8042 reset)
   * - this is how libkrun detects the guest wants to exit
   */
  sync();
  reboot(RB_AUTOBOOT);

  return exit_code;
}
