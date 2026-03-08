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

/* Scan virtio-ports for krun-stdin/stdout/stderr and redirect fds.
 *
 * The virtio console port discovery is asynchronous: the host-guest
 * handshake (DEVICE_READY -> PORT_ADD -> PORT_READY -> PORT_NAME ->
 * PORT_OPEN) can lag behind the guest init. When VMSH_REDIRECT is
 * set, we know how many ports to expect and poll until they appear
 * (up to 500 ms).
 */
static void setup_redirects(void) {
  const char *env = getenv("VMSH_REDIRECT");
  int expected = env ? atoi(env) : 0;
  if (expected <= 0)
    return; /* terminal mode -- nothing to redirect */

  /* The number of FDs successfully dup2'd. */
  int redirected = 0;
  int done[3] = {0, 0, 0}; /* per-FD flags */
  const struct timespec delay = {.tv_sec = 0, .tv_nsec = 1000000}; /* 1 ms */

  for (int attempt = 0; attempt < 500 && redirected < expected; attempt++) {
    if (attempt > 0)
      nanosleep(&delay, NULL);

    DIR *dir = opendir("/sys/class/virtio-ports");
    if (!dir)
      continue;

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

      int target_fd;
      if (strcmp(port_name, "krun-stdin") == 0)
        target_fd = STDIN_FILENO;
      else if (strcmp(port_name, "krun-stdout") == 0)
        target_fd = STDOUT_FILENO;
      else if (strcmp(port_name, "krun-stderr") == 0)
        target_fd = STDERR_FILENO;
      else
        continue;

      if (done[target_fd])
        continue;

      char dev_path[512];
      snprintf(dev_path, sizeof(dev_path), "/dev/%s", entry->d_name);

      int flags = (target_fd == STDIN_FILENO) ? O_RDONLY : O_WRONLY;
      int fd = open(dev_path, flags);
      if (fd >= 0) {
        dup2(fd, target_fd);
        if (fd != target_fd)
          close(fd);
        done[target_fd] = 1;
        redirected++;
      }
    }
    closedir(dir);
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
   */
  int exec_argc = 1 + (argc - 1);
  char **exec_argv = malloc((exec_argc + 1) * sizeof(char *));
  if (!exec_argv) {
    fprintf(stderr, "vmsh-init: malloc failed\n");
    set_exit_code(125);
    return 125;
  }
  exec_argv[0] = (char *)krun_init;
  for (int i = 1; i < argc; i++)
    exec_argv[i] = argv[i];
  exec_argv[exec_argc] = NULL;

  /* Check if we should run directly as PID 1 (no fork). */
  const char *init_pid1 = getenv("KRUN_INIT_PID1");
  if (init_pid1 && strcmp(init_pid1, "1") == 0) {
    setup_redirects();
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
