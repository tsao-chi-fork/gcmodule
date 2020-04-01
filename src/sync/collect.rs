use super::ref_count::ThreadedRefCount;
use super::Acc;
use crate::cc::CcDummy;
use crate::cc::CcDyn;
use crate::collect;
use crate::collect::Linked;
use crate::collect::ObjectSpace;
use crate::debug;
use crate::Trace;
use parking_lot::Mutex;
use parking_lot::RwLock;
use std::cell::Cell;
use std::mem;
use std::ops::Deref;
use std::pin::Pin;
use std::sync::Arc;

#[repr(C)]
pub struct Header {
    next: Cell<*const Header>,
    prev: Cell<*const Header>,

    /// Vtable of (`&CcBox<T> as &dyn CcDyn`)
    ccdyn_vptr: Cell<*mut ()>,

    lock: Arc<Mutex<()>>,
}

pub struct AccObjectSpace {
    /// Linked list to the tracked objects.
    list: Pin<Box<Header>>,

    /// Whether the collector is running.
    collecting: Arc<RwLock<()>>,
}

// safety: accesses are protected by mutex
unsafe impl Send for AccObjectSpace {}
unsafe impl Sync for AccObjectSpace {}

impl ObjectSpace for AccObjectSpace {
    type RefCount = ThreadedRefCount;
    type Header = Header;

    fn insert(&self, header: &Self::Header, value: &dyn CcDyn) {
        debug_assert!(Arc::ptr_eq(&header.lock, &self.list.lock));
        // Should be locked by `create()` already.
        debug_assert!(self.list.lock.try_lock().is_none());
        let header: &Header = &header;
        let prev: &Header = &self.list;
        debug_assert!(!collect::is_collecting(prev));
        debug_assert!(header.next.get().is_null());
        let next = prev.next.get();
        header.prev.set(prev.deref());
        header.next.set(next);
        unsafe {
            // safety: The linked list is maintained, and pointers are valid.
            (&*next).prev.set(header);
            // safety: To access vtable pointer. Test by test_gc_header_value.
            let fat_ptr: [*mut (); 2] = mem::transmute(value);
            header.ccdyn_vptr.set(fat_ptr[1]);
        }
        prev.next.set(header);
    }

    #[inline]
    fn remove(header: &Self::Header) {
        let _linked_list_lock = header.lock.lock();
        let header: &Header = &header;
        debug_assert!(!collect::is_collecting(header));
        debug_assert!(!header.next.get().is_null());
        debug_assert!(!header.prev.get().is_null());
        let next = header.next.get();
        let prev = header.prev.get();
        // safety: The linked list is maintained. Pointers in it are valid.
        unsafe {
            (*prev).next.set(next);
            (*next).prev.set(prev);
        }
        header.next.set(std::ptr::null_mut());
    }

    #[inline]
    fn new_ref_count(&self, tracked: bool) -> Self::RefCount {
        ThreadedRefCount::new(tracked, self.collecting.clone())
    }

    fn default_header(&self) -> Self::Header {
        let lock = self.list.lock.clone();
        Self::Header {
            lock,
            next: Cell::new(std::ptr::null()),
            prev: Cell::new(std::ptr::null()),
            ccdyn_vptr: Cell::new(CcDummy::ccdyn_vptr()),
        }
    }
}

impl Default for AccObjectSpace {
    /// Constructs an empty [`AccObjectSpace`](struct.AccObjectSpace.html).
    fn default() -> Self {
        let lock = Arc::new(Mutex::new(()));
        let pinned = Box::pin(Header {
            prev: Cell::new(std::ptr::null()),
            next: Cell::new(std::ptr::null()),
            ccdyn_vptr: Cell::new(CcDummy::ccdyn_vptr()),
            lock,
        });
        let header: &Header = &pinned;
        header.prev.set(header);
        header.next.set(header);
        Self {
            list: pinned,
            collecting: Default::default(),
        }
    }
}

impl AccObjectSpace {
    /// Count objects tracked by this [`ObjectSpace`](struct.ObjectSpace.html).
    pub fn count_tracked(&self) -> usize {
        let _linked_list_lock = self.list.lock.lock();
        let list: &Header = &self.list;
        let mut count = 0;
        collect::visit_list(list, |_| count += 1);
        count
    }

    /// Collect cyclic garbage tracked by this [`ObjectSpace`](struct.ObjectSpace.html).
    /// Return the number of objects collected.
    pub fn collect_cycles(&self) -> usize {
        // Block (and wait for) deref.
        let ref_lock = self.collecting.write();
        // Block linked list changes.
        let linked_list_lock = self.list.lock.lock();
        debug::log(|| ("AccObjectSpace", "start collect_cycles with lock"));
        let list: &Header = &self.list;
        let result = collect::collect_list(list, (linked_list_lock, ref_lock));
        debug::log(|| ("AccObjectSpace", "end collect_cycles"));
        result
    }

    /// Constructs a new [`Acc<T>`](struct.Acc.html) in this
    /// [`AccObjectSpace`](struct.AccObjectSpace.html).
    ///
    /// The returned [`Acc<T>`](struct.Cc.html) can refer to other
    ///  `Acc`s in the same [`AccObjectSpace`](struct.AccObjectSpace.html).
    ///
    /// If an `Acc` refers to another `Acc` in another
    /// [`AccObjectSpace`](struct.AccObjectSpace.html), the cyclic collector
    /// will not be able to collect cycles.
    pub fn create<T: Trace>(&self, value: T) -> Acc<T> {
        let _linked_list_lock = self.list.lock.lock();
        Acc::new_in_space(value, self)
    }
}

impl Linked for Header {
    #[inline]
    fn next(&self) -> *const Self {
        self.next.get()
    }
    #[inline]
    fn prev(&self) -> *const Self {
        self.prev.get()
    }
    #[inline]
    fn set_prev(&self, other: *const Self) {
        self.prev.set(other)
    }
    #[inline]
    fn value(&self) -> &dyn CcDyn {
        // safety: To build trait object from self and vtable pointer.
        // Test by test_gc_header_value_consistency().
        unsafe {
            let fat_ptr: (*const (), *mut ()) =
                ((self as *const Self).offset(1) as _, self.ccdyn_vptr.get());
            mem::transmute(fat_ptr)
        }
    }
}