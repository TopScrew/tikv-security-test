// Copyright 2020 TiKV Project Authors. Licensed under Apache-2.0.

define_error_codes!(
    "KV:Storage:",

    TIMEOUT => ("Timeout", "", ""),
    EMPTY_REQUEST => ("EmptyRequest", "", ""),
    CLOSED => ("Closed", "", ""),
    IO => ("Io", "", ""),
    SCHED_TOO_BUSY => ("SchedTooBusy", "", ""),
    GC_WORKER_TOO_BUSY => ("GcWorkerTooBusy", "", ""),
    KEY_TOO_LARGE => ("KeyTooLarge", "", ""),
    INVALID_CF => ("InvalidCf", "", ""),
    CF_DEPRECATED => ("CfDeprecated", "", ""),
    TTL_NOT_ENABLED => ("TtlNotEnabled", "", ""),
    TTL_LEN_NOT_EQUALS_TO_PAIRS => ("TtlLenNotEqualsToPairs", "", ""),
    PROTOBUF => ("Protobuf", "", ""),
    INVALID_TXN_TSO => ("InvalidTxnTso", "", ""),
    INVALID_REQ_RANGE => ("InvalidReqRange", "", ""),
    BAD_FORMAT_LOCK => ("BadFormatLock", "", ""),
    BAD_FORMAT_WRITE => ("BadFormatWrite", "",""),
    KEY_IS_LOCKED => ("KeyIsLocked", "", ""),
    MAX_TIMESTAMP_NOT_SYNCED => ("MaxTimestampNotSynced", "", ""),
    FLASHBACK_NOT_PREPARED => ("FlashbackNotPrepared", "", ""),
    DEADLINE_EXCEEDED => ("DeadlineExceeded", "", ""),
    API_VERSION_NOT_MATCHED => ("ApiVersionNotMatched", "", ""),
    INVALID_KEY_MODE => ("InvalidKeyMode", "", ""),
    INVALID_MAX_TS_UPDATE => ("InvalidMaxTsUpdate", "", ""),

    COMMITTED => ("Committed", "", ""),
    PESSIMISTIC_LOCK_ROLLED_BACK => ("PessimisticLockRolledBack", "", ""),
    TXN_LOCK_NOT_FOUND => ("TxnLockNotFound", "", ""),
    TXN_NOT_FOUND => ("TxnNotFound", "", ""),
    LOCK_TYPE_NOT_MATCH => ("LockTypeNotMatch", "", ""),
    WRITE_CONFLICT => ("WriteConflict", "", ""),
    DEADLOCK => ("Deadlock", "", ""),
    ALREADY_EXIST => ("AlreadyExist", "",""),
    DEFAULT_NOT_FOUND => ("DefaultNotFound", "", ""),
    COMMIT_TS_EXPIRED => ("CommitTsExpired", "", ""),
    KEY_VERSION => ("KeyVersion", "",""),
    PESSIMISTIC_LOCK_NOT_FOUND => ("PessimisticLockNotFound", "", ""),
    COMMIT_TS_TOO_LARGE => ("CommitTsTooLarge", "", ""),

    ASSERTION_FAILED => ("AssertionFailed", "", ""),
    LOCK_IF_EXISTS_FAILED => ("LockIfExistsFailed", "", ""),

    PRIMARY_MISMATCH => ("PrimaryMismatch", "", ""),
    UNDETERMINED => ("Undetermined", "", ""),

    UNKNOWN => ("Unknown", "", "")
);
