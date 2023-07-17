#!/usr/sbin/dtrace -Cs

#pragma D option bufsize=64m
#pragma D option switchrate=501hz
#pragma D option quiet

#define	P_PFX()		printf("%19u pid %8u tid %5u [%-16s] ", \
			    walltimestamp, pid, tid, execname)

#define	P_ERRNO()	printf(" errno %d (%s)", errno, errname[errno])
#define P_PATH(ffdd)	printf(" path %s", fds[ffdd].fi_pathname)

inline string errname[int r] =
    r == EPERM ? "EPERM" :
    r == ENOENT ? "ENOENT" :
    r == ESRCH ? "ESRCH" :
    r == EINTR ? "EINTR" :
    r == EIO ? "EIO" :
    r == ENXIO ? "ENXIO" :
    r == E2BIG ? "E2BIG" :
    r == ENOEXEC ? "ENOEXEC" :
    r == EBADF ? "EBADF" :
    r == ECHILD ? "ECHILD" :
    r == EAGAIN ? "EAGAIN" :
    r == ENOMEM ? "ENOMEM" :
    r == EACCES ? "EACCES" :
    r == EFAULT ? "EFAULT" :
    r == ENOTBLK ? "ENOTBLK" :
    r == EBUSY ? "EBUSY" :
    r == EEXIST ? "EEXIST" :
    r == EXDEV ? "EXDEV" :
    r == ENODEV ? "ENODEV" :
    r == ENOTDIR ? "ENOTDIR" :
    r == EISDIR ? "EISDIR" :
    r == EINVAL ? "EINVAL" :
    r == ENFILE ? "ENFILE" :
    r == EMFILE ? "EMFILE" :
    r == ENOTTY ? "ENOTTY" :
    r == ETXTBSY ? "ETXTBSY" :
    r == EFBIG ? "EFBIG" :
    r == ENOSPC ? "ENOSPC" :
    r == ESPIPE ? "ESPIPE" :
    r == EROFS ? "EROFS" :
    r == EMLINK ? "EMLINK" :
    r == EPIPE ? "EPIPE" :
    r == EDOM ? "EDOM" :
    r == ERANGE ? "ERANGE" :
    r == ENOMSG ? "ENOMSG" :
    r == EIDRM ? "EIDRM" :
    r == ECHRNG ? "ECHRNG" :
    r == EL2NSYNC ? "EL2NSYNC" :
    r == EL3HLT ? "EL3HLT" :
    r == EL3RST ? "EL3RST" :
    r == ELNRNG ? "ELNRNG" :
    r == EUNATCH ? "EUNATCH" :
    r == ENOCSI ? "ENOCSI" :
    r == EL2HLT ? "EL2HLT" :
    r == EDEADLK ? "EDEADLK" :
    r == ENOLCK ? "ENOLCK" :
    r == ECANCELED ? "ECANCELED" :
    r == ENOTSUP ? "ENOTSUP" :
    r == EDQUOT ? "EDQUOT" :
    r == EBADE ? "EBADE" :
    r == EBADR ? "EBADR" :
    r == EXFULL ? "EXFULL" :
    r == ENOANO ? "ENOANO" :
    r == EBADRQC ? "EBADRQC" :
    r == EBADSLT ? "EBADSLT" :
    r == EDEADLOCK ? "EDEADLOCK" :
    r == EBFONT ? "EBFONT" :
    r == EOWNERDEAD ? "EOWNERDEAD" :
    r == ENOTRECOVERABLE ? "ENOTRECOVERABLE" :
    r == ENOSTR ? "ENOSTR" :
    r == ENODATA ? "ENODATA" :
    r == ETIME ? "ETIME" :
    r == ENOSR ? "ENOSR" :
    r == ENONET ? "ENONET" :
    r == ENOPKG ? "ENOPKG" :
    r == EREMOTE ? "EREMOTE" :
    r == ENOLINK ? "ENOLINK" :
    r == EADV ? "EADV" :
    r == ESRMNT ? "ESRMNT" :
    r == ECOMM ? "ECOMM" :
    r == EPROTO ? "EPROTO" :
    r == ELOCKUNMAPPED ? "ELOCKUNMAPPED" :
    r == ENOTACTIVE ? "ENOTACTIVE" :
    r == EMULTIHOP ? "EMULTIHOP" :
    r == EBADMSG ? "EBADMSG" :
    r == ENAMETOOLONG ? "ENAMETOOLONG" :
    r == EOVERFLOW ? "EOVERFLOW" :
    r == ENOTUNIQ ? "ENOTUNIQ" :
    r == EBADFD ? "EBADFD" :
    r == EREMCHG ? "EREMCHG" :
    r == ELIBACC ? "ELIBACC" :
    r == ELIBBAD ? "ELIBBAD" :
    r == ELIBSCN ? "ELIBSCN" :
    r == ELIBMAX ? "ELIBMAX" :
    r == ELIBEXEC ? "ELIBEXEC" :
    r == EILSEQ ? "EILSEQ" :
    r == ENOSYS ? "ENOSYS" :
    r == ELOOP ? "ELOOP" :
    r == ERESTART ? "ERESTART" :
    r == ESTRPIPE ? "ESTRPIPE" :
    r == ENOTEMPTY ? "ENOTEMPTY" :
    r == EUSERS ? "EUSERS" :
    r == ENOTSOCK ? "ENOTSOCK" :
    r == EDESTADDRREQ ? "EDESTADDRREQ" :
    r == EMSGSIZE ? "EMSGSIZE" :
    r == EPROTOTYPE ? "EPROTOTYPE" :
    r == ENOPROTOOPT ? "ENOPROTOOPT" :
    r == EPROTONOSUPPORT ? "EPROTONOSUPPORT" :
    r == ESOCKTNOSUPPORT ? "ESOCKTNOSUPPORT" :
    r == EOPNOTSUPP ? "EOPNOTSUPP" :
    r == EPFNOSUPPORT ? "EPFNOSUPPORT" :
    r == EAFNOSUPPORT ? "EAFNOSUPPORT" :
    r == EADDRINUSE ? "EADDRINUSE" :
    r == EADDRNOTAVAIL ? "EADDRNOTAVAIL" :
    r == ENETDOWN ? "ENETDOWN" :
    r == ENETUNREACH ? "ENETUNREACH" :
    r == ENETRESET ? "ENETRESET" :
    r == ECONNABORTED ? "ECONNABORTED" :
    r == ECONNRESET ? "ECONNRESET" :
    r == ENOBUFS ? "ENOBUFS" :
    r == EISCONN ? "EISCONN" :
    r == ENOTCONN ? "ENOTCONN" :
    r == ESHUTDOWN ? "ESHUTDOWN" :
    r == ETOOMANYREFS ? "ETOOMANYREFS" :
    r == ETIMEDOUT ? "ETIMEDOUT" :
    r == ECONNREFUSED ? "ECONNREFUSED" :
    r == EHOSTDOWN ? "EHOSTDOWN" :
    r == EHOSTUNREACH ? "EHOSTUNREACH" :
    r == EWOULDBLOCK ? "EWOULDBLOCK" :
    r == EALREADY ? "EALREADY" :
    r == EINPROGRESS ? "EINPROGRESS" :
    r == ESTALE ? "ESTALE" :
    "<?>";

syscall::open:entry
{
	/*
	 * Save the usermode address of the input path in a thread-local:
	 */
	self->open_addr = arg0;

	/*
	 * Attempt to copy the usermode path in for inclusion in messages.  If
	 * this fails due to some memory fault (e.g., if a process was using a
	 * read-only constant for a path, and that page has not yet been
	 * faulted into the child) then this will fail and the rest of the
	 * enabling will not run.  That's OK in this case, as we do the work
	 * (based on whether or not this clause-local is set) in the next
	 * clause.
	 */
	this->open_path = copyinstr(arg0);
}

syscall::open:entry
{
	self->open_path = this->open_path;
	self->open = 1;

	/*
	 * NOTE: if-else statements produce separate enablings, so all printf
	 * statements MUST occur within a single block, or they could be
	 * intermingled with output from other clauses.
	 */
	if (self->open_path != 0) {
		P_PFX();
		printf("open(%s)\n", self->open_path);
	} else {
		P_PFX();
		printf("open(0x%x)\n", self->open_addr);
	}
}

syscall::open:return
/self->open != 0/
{
	this->r = (int)arg1;

	/*
	 * NOTE: if-else statements produce separate enablings, so all printf
	 * statements MUST occur within a single block, or they could be
	 * intermingled with output from other clauses.
	 */
	if (self->open_path != 0 && this->r < 0) {
		P_PFX();
		printf("open(%s) -> %d", self->open_path, (int)arg1);
		P_ERRNO();
		printf("\n");
	} else if (self->open_path != 0 && this-> r >= 0) {
		P_PFX();
		printf("open(%s) -> %d", self->open_path, (int)arg1);
		P_PATH(this->r);
		printf("\n");
	} else if (self->open_path == 0 && this->r < 0) {
		P_PFX();
		printf("open(0x%x) -> %d", self->open_addr, (int)arg1);
		P_ERRNO();
		printf("\n");
	} else {
		P_PFX();
		printf("open(0x%x) -> %d", self->open_addr, (int)arg1);
		P_PATH(this->r);
		printf("\n");
	}

	self->open_addr = 0;
	self->open_path = 0;
	self->open = 0;
}

syscall::close:entry
{
	self->close = 1;
	self->close_fd = (int)arg0;

	P_PFX();
	printf("close(%d) path %s\n", self->close_fd,
	    fds[self->close_fd].fi_pathname);
}

syscall::close:return
/self->close != 0/
{
	this->r = (int)arg1;

	/*
	 * NOTE: if-else statements produce separate enablings, so all printf
	 * statements MUST occur within a single block, or they could be
	 * intermingled with output from other clauses.
	 */
	if (this->r < 0) {
		P_PFX();
		printf("close(%d) -> %d", self->close_fd, (int)arg1);
		P_ERRNO();
		printf("\n");
	} else {
		P_PFX();
		printf("close(%d) -> %d", self->close_fd, (int)arg1);
		P_PATH(this->r);
		printf("\n");
	}

	self->close_fd = 0;
	self->close = 0;
}

proc:::create
{
	P_PFX();
	printf("starting child; pid %d\n", args[0]->pr_pid);
}

proc:::start
{
	P_PFX();
	printf("child started\n");
}

proc:::exec-success
{
	P_PFX();
	printf("exec ok; args \"%s\"\n", curpsinfo->pr_psargs);
}

proc:::exit
{
	P_PFX();
	printf("exit; code %d (0x%x)\n", args[0], args[0]);
}
