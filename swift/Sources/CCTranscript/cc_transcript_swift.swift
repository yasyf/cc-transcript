import RustXcframework
public func session_activity<GenericIntoRustString: IntoRustString>(_ path: GenericIntoRustString, _ waiting_tools: RustVec<GenericIntoRustString>, _ human_facing_tools: RustVec<GenericIntoRustString>) throws -> SessionActivity {
    try { let val = __swift_bridge__$session_activity({ let rustString = path.intoRustString(); rustString.isOwned = false; return rustString.ptr }(), { let val = waiting_tools; val.isOwned = false; return val.ptr }(), { let val = human_facing_tools; val.isOwned = false; return val.ptr }()); if val.is_ok { return SessionActivity(ptr: val.ok_or_err!) } else { throw RustString(ptr: val.ok_or_err!) } }()
}

public class SessionActivity: SessionActivityRefMut {
    var isOwned: Bool = true

    public override init(ptr: UnsafeMutableRawPointer) {
        super.init(ptr: ptr)
    }

    deinit {
        if isOwned {
            __swift_bridge__$SessionActivity$_free(ptr)
        }
    }
}
public class SessionActivityRefMut: SessionActivityRef {
    public override init(ptr: UnsafeMutableRawPointer) {
        super.init(ptr: ptr)
    }
}
public class SessionActivityRef {
    var ptr: UnsafeMutableRawPointer

    public init(ptr: UnsafeMutableRawPointer) {
        self.ptr = ptr
    }
}
extension SessionActivityRef {
    public func is_waiting() -> Bool {
        __swift_bridge__$SessionActivity$is_waiting(ptr)
    }

    public func mid_tool() -> Bool {
        __swift_bridge__$SessionActivity$mid_tool(ptr)
    }

    public func last_event_epoch() -> Optional<Int64> {
        __swift_bridge__$SessionActivity$last_event_epoch(ptr).intoSwiftRepr()
    }

    public func pending() -> RustVec<PendingItem> {
        RustVec(ptr: __swift_bridge__$SessionActivity$pending(ptr))
    }
}
extension SessionActivity: Vectorizable {
    public static func vecOfSelfNew() -> UnsafeMutableRawPointer {
        __swift_bridge__$Vec_SessionActivity$new()
    }

    public static func vecOfSelfFree(vecPtr: UnsafeMutableRawPointer) {
        __swift_bridge__$Vec_SessionActivity$drop(vecPtr)
    }

    public static func vecOfSelfPush(vecPtr: UnsafeMutableRawPointer, value: SessionActivity) {
        __swift_bridge__$Vec_SessionActivity$push(vecPtr, {value.isOwned = false; return value.ptr;}())
    }

    public static func vecOfSelfPop(vecPtr: UnsafeMutableRawPointer) -> Optional<Self> {
        let pointer = __swift_bridge__$Vec_SessionActivity$pop(vecPtr)
        if pointer == nil {
            return nil
        } else {
            return (SessionActivity(ptr: pointer!) as! Self)
        }
    }

    public static func vecOfSelfGet(vecPtr: UnsafeMutableRawPointer, index: UInt) -> Optional<SessionActivityRef> {
        let pointer = __swift_bridge__$Vec_SessionActivity$get(vecPtr, index)
        if pointer == nil {
            return nil
        } else {
            return SessionActivityRef(ptr: pointer!)
        }
    }

    public static func vecOfSelfGetMut(vecPtr: UnsafeMutableRawPointer, index: UInt) -> Optional<SessionActivityRefMut> {
        let pointer = __swift_bridge__$Vec_SessionActivity$get_mut(vecPtr, index)
        if pointer == nil {
            return nil
        } else {
            return SessionActivityRefMut(ptr: pointer!)
        }
    }

    public static func vecOfSelfAsPtr(vecPtr: UnsafeMutableRawPointer) -> UnsafePointer<SessionActivityRef> {
        UnsafePointer<SessionActivityRef>(OpaquePointer(__swift_bridge__$Vec_SessionActivity$as_ptr(vecPtr)))
    }

    public static func vecOfSelfLen(vecPtr: UnsafeMutableRawPointer) -> UInt {
        __swift_bridge__$Vec_SessionActivity$len(vecPtr)
    }
}


public class PendingItem: PendingItemRefMut {
    var isOwned: Bool = true

    public override init(ptr: UnsafeMutableRawPointer) {
        super.init(ptr: ptr)
    }

    deinit {
        if isOwned {
            __swift_bridge__$PendingItem$_free(ptr)
        }
    }
}
public class PendingItemRefMut: PendingItemRef {
    public override init(ptr: UnsafeMutableRawPointer) {
        super.init(ptr: ptr)
    }
}
public class PendingItemRef {
    var ptr: UnsafeMutableRawPointer

    public init(ptr: UnsafeMutableRawPointer) {
        self.ptr = ptr
    }
}
extension PendingItemRef {
    public func tool_use_id() -> Optional<RustString> {
        { let val = __swift_bridge__$PendingItem$tool_use_id(ptr); if val != nil { return RustString(ptr: val!) } else { return nil } }()
    }

    public func name() -> RustStr {
        __swift_bridge__$PendingItem$name(ptr)
    }

    public func kind() -> RustStr {
        __swift_bridge__$PendingItem$kind(ptr)
    }
}
extension PendingItem: Vectorizable {
    public static func vecOfSelfNew() -> UnsafeMutableRawPointer {
        __swift_bridge__$Vec_PendingItem$new()
    }

    public static func vecOfSelfFree(vecPtr: UnsafeMutableRawPointer) {
        __swift_bridge__$Vec_PendingItem$drop(vecPtr)
    }

    public static func vecOfSelfPush(vecPtr: UnsafeMutableRawPointer, value: PendingItem) {
        __swift_bridge__$Vec_PendingItem$push(vecPtr, {value.isOwned = false; return value.ptr;}())
    }

    public static func vecOfSelfPop(vecPtr: UnsafeMutableRawPointer) -> Optional<Self> {
        let pointer = __swift_bridge__$Vec_PendingItem$pop(vecPtr)
        if pointer == nil {
            return nil
        } else {
            return (PendingItem(ptr: pointer!) as! Self)
        }
    }

    public static func vecOfSelfGet(vecPtr: UnsafeMutableRawPointer, index: UInt) -> Optional<PendingItemRef> {
        let pointer = __swift_bridge__$Vec_PendingItem$get(vecPtr, index)
        if pointer == nil {
            return nil
        } else {
            return PendingItemRef(ptr: pointer!)
        }
    }

    public static func vecOfSelfGetMut(vecPtr: UnsafeMutableRawPointer, index: UInt) -> Optional<PendingItemRefMut> {
        let pointer = __swift_bridge__$Vec_PendingItem$get_mut(vecPtr, index)
        if pointer == nil {
            return nil
        } else {
            return PendingItemRefMut(ptr: pointer!)
        }
    }

    public static func vecOfSelfAsPtr(vecPtr: UnsafeMutableRawPointer) -> UnsafePointer<PendingItemRef> {
        UnsafePointer<PendingItemRef>(OpaquePointer(__swift_bridge__$Vec_PendingItem$as_ptr(vecPtr)))
    }

    public static func vecOfSelfLen(vecPtr: UnsafeMutableRawPointer) -> UInt {
        __swift_bridge__$Vec_PendingItem$len(vecPtr)
    }
}



