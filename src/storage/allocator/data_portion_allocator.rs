use std::cmp;
use std::collections::BTreeSet;
use std::collections::Bound::{Excluded, Included, Unbounded};

use super::free_portion::{EndBasedFreePortion, FreePortion, SizeBasedFreePortion};
use super::U24;
use metrics::DataAllocatorMetrics;
use storage::portion::DataPortion;
use storage::Address;
use {ErrorKind, Result};

/// データ領域用のアロケータ.
///
/// 指定された容量を有するデータ領域から、個々のlumpに必要な部分領域の割当を担当する.
///
/// 割当の単位は"バイト"ではなく、"ブロック"となる.
/// (ただし、これをアロケータのレイヤーで意識する必要はない)
///
/// この実装自体は、完全にメモリ上のデータ構造であり、状態は永続化されない.
///
/// ただし、部分領域群の割当状況自体はジャーナル領域に
/// 情報が残っているので、アロケータインスタンスの構築時には、そこから前回の状態を復元することになる.
///
/// # 割当戦略
///
/// このアロケータは"BestFit"戦略を採用している.
///
/// "BestFit"戦略では、空き領域のリストを管理している.
///
/// 新規割当要求が発行された際には、空き領域のリストを探索し、
/// 要求サイズを満たす空き領域の中で、一番サイズが小さいものが選択される.
///
/// 選択された空き領域は、その中から要求サイズ分だけの割当を行い、
/// もしまだ余剰分がある場合には、再び空き領域リストに戻される.
#[derive(Debug)]
pub struct DataPortionAllocator {
    size_to_free: BTreeSet<SizeBasedFreePortion>,
    end_to_free: BTreeSet<EndBasedFreePortion>,
    metrics: DataAllocatorMetrics,
}
impl DataPortionAllocator {
    /// アロケータを構築する.
    ///
    /// `portions`には、既に割当済みの部分領域群が列挙されている.
    ///
    /// アロケータが利用可能な領域のサイズ（キャパシティ）の情報は、`metrics`から取得される.
    pub fn build<I>(metrics: DataAllocatorMetrics, portions: I) -> Result<Self>
    where
        I: Iterator<Item = DataPortion>,
    {
        let block_size = u64::from(metrics.block_size.as_u16());
        let mut portions = portions.collect::<Vec<_>>();
        metrics
            .allocated_portions_at_starting
            .add_u64(portions.len() as u64);
        metrics
            .allocated_bytes_at_starting
            .add_u64(portions.iter().map(|p| u64::from(p.len) * block_size).sum());

        let sentinel = DataPortion {
            start: Address::from(0),
            len: 0,
        };
        portions.push(sentinel);
        portions.sort();
        portions.reverse();

        let mut tail = metrics.capacity_bytes / block_size;
        let mut allocator = DataPortionAllocator {
            size_to_free: BTreeSet::new(),
            end_to_free: BTreeSet::new(),
            metrics,
        };
        for portion in portions {
            track_assert!(portion.end().as_u64() <= tail, ErrorKind::InvalidInput);
            while portion.end().as_u64() < tail {
                let delta = tail - portion.end().as_u64();
                let size = cmp::min(0xFF_FFFF, delta) as U24;

                tail -= u64::from(size);
                let free = FreePortion::new(Address::from_u64(tail).unwrap(), size);
                allocator.add_free_portion(free);
            }
            tail = portion.start.as_u64();
        }
        Ok(allocator)
    }

    /// `size`分の部分領域の割当を行う.
    ///
    /// 十分な領域が存在しない場合には`None`が返される.
    pub fn allocate(&mut self, size: u16) -> Option<DataPortion> {
        let portion = SizeBasedFreePortion(FreePortion::new(Address::from(0), U24::from(size)));
        if let Some(mut free) = self
            .size_to_free
            .range((Included(&portion), Unbounded))
            .nth(0)
            .map(|p| p.0)
        {
            debug_assert!(U24::from(size) <= free.len());
            self.delete_free_portion(free);
            let allocated = free.allocate(size);
            if free.len() > 0 {
                self.add_free_portion(free);
            }
            self.metrics.count_allocation(allocated.len);
            Some(allocated)
        } else {
            self.metrics.nospace_failures.increment();
            None
        }
    }

    /// 割当済みの部分領域の解放を行う.
    ///
    /// # 事前条件
    ///
    /// - `portion`は「以前に割当済み」かつ「未解放」の部分領域である
    pub fn release(&mut self, portion: DataPortion) {
        assert!(self.is_allocated_portion(&portion), "{:?}", portion);
        self.metrics.count_releasion(portion.len);
        let portion = self.merge_free_portions_if_possible(FreePortion::from(portion));
        self.add_free_portion(portion);
    }

    /// アロケータ用のメトリクスを返す.
    pub fn metrics(&self) -> &DataAllocatorMetrics {
        &self.metrics
    }

    fn add_free_portion(&mut self, portion: FreePortion) {
        assert!(self.size_to_free.insert(SizeBasedFreePortion(portion)));
        assert!(self.end_to_free.insert(EndBasedFreePortion(portion)));
        self.metrics.inserted_free_portions.increment();
    }

    fn delete_free_portion(&mut self, portion: FreePortion) {
        assert!(self.size_to_free.remove(&SizeBasedFreePortion(portion)));
        assert!(self.end_to_free.remove(&EndBasedFreePortion(portion)));
        self.metrics.removed_free_portions.increment();
    }

    // `portion`と隣接する領域がフリーリスト内に存在する場合には、それらをまとめてしまう.
    fn merge_free_portions_if_possible(&mut self, mut portion: FreePortion) -> FreePortion {
        // 「`portion`の始端」と「既存の空き領域の終端」が一致している
        let key = FreePortion::new(portion.start(), 0);
        if let Some(prev) = self.end_to_free.get(&EndBasedFreePortion(key)).map(|p| p.0) {
            if portion.checked_extend(prev.len()) {
                portion = FreePortion::new(prev.start(), portion.len());
                self.delete_free_portion(prev);
            }
        }

        // 「`portion`の終端」と「既存の空き領域の始端」が一致している
        let key = FreePortion::new(portion.end(), 0);
        if let Some(next) = self
            .end_to_free
            .range((Excluded(&EndBasedFreePortion(key)), Unbounded))
            .nth(0)
            .map(|p| p.0)
        {
            if next.start() == portion.end() && portion.checked_extend(next.len()) {
                self.delete_free_portion(next);
            }
        }

        portion
    }

    fn is_allocated_portion(&self, portion: &DataPortion) -> bool {
        // フリーリスト内のいずれとも領域が重なっていない場合には、割当済み領域だと判断する
        let key = EndBasedFreePortion(FreePortion::new(portion.start, 0));
        if let Some(next) = self.end_to_free.range((Excluded(&key), Unbounded)).nth(0) {
            portion.end() <= next.0.start()
        } else {
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use prometrics::metrics::MetricBuilder;
    use std::iter;
    use trackable::result::TestResult;

    use block::BlockSize;
    use lump::LumpId;
    use metrics::DataAllocatorMetrics;
    use storage::allocator::DataPortionAllocator;
    use storage::index::LumpIndex;
    use storage::portion::{DataPortion, Portion};
    use storage::Address;

    #[test]
    fn it_works() -> TestResult {
        let capacity = Address::from(24);
        let mut allocator = track!(DataPortionAllocator::build(
            metrics(capacity),
            iter::empty()
        ))?;
        assert_eq!(allocator.allocate(10), Some(portion(0, 10)));
        assert_eq!(allocator.allocate(10), Some(portion(10, 10)));
        assert_eq!(allocator.allocate(10), None);
        assert_eq!(allocator.allocate(4), Some(portion(20, 4)));

        allocator.release(portion(10, 10));
        assert_eq!(allocator.allocate(5), Some(portion(10, 5)));
        assert_eq!(allocator.allocate(2), Some(portion(15, 2)));
        assert_eq!(allocator.allocate(4), None);

        let m = allocator.metrics();
        assert_eq!(m.free_list_len(), 1);
        assert_eq!(m.allocated_portions(), 5);
        assert_eq!(m.released_portions(), 1);
        assert_eq!(m.nospace_failures(), 2);
        assert_eq!(m.usage_bytes(), 21 * u64::from(BlockSize::MIN));
        assert_eq!(m.capacity_bytes, 24 * u64::from(BlockSize::MIN));
        Ok(())
    }

    #[test]
    #[should_panic]
    fn it_panics() {
        let capacity = Address::from(24);
        let mut allocator = DataPortionAllocator::build(metrics(capacity), iter::empty())
            .expect("Unexpected panic");

        // Try releasing an unallocated portion
        allocator.release(portion(10, 10));
    }

    #[test]
    fn rebuild() -> TestResult {
        let mut index = LumpIndex::new();
        index.insert(lump_id("000"), Portion::Data(portion(5, 10)));
        index.insert(lump_id("111"), Portion::Data(portion(2, 3)));
        index.insert(lump_id("222"), Portion::Data(portion(15, 5)));
        index.remove(&lump_id("000"));

        let capacity = Address::from(20);
        let mut allocator = track!(DataPortionAllocator::build(
            metrics(capacity),
            index.data_portions()
        ))?;
        assert_eq!(allocator.metrics().free_list_len(), 2);
        assert_eq!(allocator.metrics().allocated_portions(), 2);

        assert_eq!(allocator.allocate(11), None);
        assert_eq!(allocator.allocate(10), Some(portion(5, 10)));
        assert_eq!(allocator.allocate(3), None);
        assert_eq!(allocator.allocate(1), Some(portion(0, 1)));
        assert_eq!(allocator.allocate(1), Some(portion(1, 1)));
        assert_eq!(allocator.allocate(1), None);
        assert_eq!(allocator.metrics().free_list_len(), 0);
        assert_eq!(
            allocator.metrics().usage_bytes(),
            allocator.metrics().capacity_bytes
        );
        Ok(())
    }

    #[test]
    fn rebuild2() -> TestResult {
        let mut index = LumpIndex::new();
        index.insert(lump_id("000"), Portion::Data(portion(1, 10)));
        index.insert(lump_id("222"), Portion::Data(portion(15, 5)));

        let capacity = Address::from(20);
        let mut allocator = track!(DataPortionAllocator::build(
            metrics(capacity),
            index.data_portions()
        ))?;

        assert_eq!(allocator.allocate(2), Some(portion(11, 2)));
        assert_eq!(allocator.allocate(1), Some(portion(0, 1)));
        Ok(())
    }

    #[test]
    fn allocate_and_release() -> TestResult {
        let capacity = Address::from(419431);
        let mut allocator = track!(DataPortionAllocator::build(
            metrics(capacity),
            iter::empty()
        ))?;

        let p0 = allocator.allocate(65).unwrap();
        let p1 = allocator.allocate(65).unwrap();
        let p2 = allocator.allocate(65).unwrap();
        allocator.release(p0);
        allocator.release(p1);

        let p3 = allocator.allocate(65).unwrap();
        let p4 = allocator.allocate(65).unwrap();
        allocator.release(p2);
        allocator.release(p3);

        let p5 = allocator.allocate(65).unwrap();
        let p6 = allocator.allocate(65).unwrap();
        allocator.release(p4);
        allocator.release(p5);
        allocator.release(p6);
        Ok(())
    }

    fn lump_id(id: &str) -> LumpId {
        id.parse().unwrap()
    }

    fn portion(offset: u32, length: u16) -> DataPortion {
        DataPortion {
            start: Address::from(offset),
            len: length,
        }
    }

    fn metrics(capacity: Address) -> DataAllocatorMetrics {
        let capacity_bytes = capacity.as_u64() * u64::from(BlockSize::MIN);
        DataAllocatorMetrics::new(&MetricBuilder::new(), capacity_bytes, BlockSize::min())
    }
}
