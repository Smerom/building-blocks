use crate::{ChunkKey, ChunkKey2, ChunkKey3, Compression};

use building_blocks_core::prelude::*;

use core::ops::RangeInclusive;
use futures::future::join_all;
use itertools::Itertools;
use sled::Tree;

/// A persistent, transactional database of chunks.
///
/// This is essentially a B+ tree of compressed chunks (backed by the `sled` crate). The keys are morton codes for the
/// corresponding chunks coordinates. This ensures that all of the chunks in an orthant are stored in a contiguous key space.
///
/// The DB values are only portable if the `compression` used respects endianness of the current machine.
pub struct ChunkDb<N, Compr> {
    tree: Tree,
    compression: Compr,
    marker: std::marker::PhantomData<N>,
}

pub trait DatabaseKey<N> {
    type Key: Copy + Ord;
    type KeyBytes: AsRef<[u8]>;

    fn into_ord_key(self) -> Self::Key;
    fn from_ord_key(key: Self::Key) -> Self;

    fn ord_key_to_be_bytes(key: Self::Key) -> Self::KeyBytes;
    fn ord_key_from_be_bytes(bytes: &[u8]) -> Self::Key;

    fn orthant_range(lod: u8, orthant: Orthant<N>) -> RangeInclusive<Self::KeyBytes>;
}

impl<N, Compr> ChunkDb<N, Compr> {
    pub fn new(tree: Tree, compression: Compr) -> Self {
        Self {
            tree,
            compression,
            marker: Default::default(),
        }
    }
}

impl<N, Compr> ChunkDb<N, Compr>
where
    ChunkKey<N>: DatabaseKey<N>,
    Compr: Compression + Copy,
{
    /// Insert a set of chunks. This will compress all of the chunks asynchronously then insert them into the database.
    /// Pre-existing chunks will be overwritten.
    pub async fn write_chunks<'a>(
        &self,
        chunks: impl Iterator<Item = (ChunkKey<N>, &'a Compr::Data)>,
    ) -> sled::Result<()>
    where
        Compr::Data: 'a,
    {
        // First compress all of the chunks in parallel.
        let mut compressed_chunks = Vec::new();
        for batch_of_chunks in &chunks.into_iter().chunks(16) {
            for (key, compressed_chunk) in
                join_all(batch_of_chunks.into_iter().map(|(key, chunk)| async move {
                    (
                        <ChunkKey<N> as DatabaseKey<N>>::into_ord_key(key),
                        self.compression.compress(&chunk),
                    )
                }))
                .await
                .into_iter()
            {
                compressed_chunks.push((key, compressed_chunk));
            }
        }
        // Sort them by the Ord key.
        compressed_chunks.sort_by_key(|(k, _)| *k);

        // Then write them to the database.
        for (db_key, chunk) in compressed_chunks.into_iter() {
            let key_bytes = ChunkKey::<N>::ord_key_to_be_bytes(db_key);
            // PERF: use a Batch instead?
            self.tree.insert(key_bytes, chunk.take_bytes())?;
        }

        Ok(())
    }

    /// Scans the given orthant for chunks, decompresses them, then returns them. Because chunk keys are stored in Morton order,
    /// the chunks in any orthant are guaranteed to be contiguous.
    ///
    /// The `orthant` is expected in voxel units, not chunk units.
    pub async fn read_chunks_in_orthant(
        &self,
        lod: u8,
        orthant: Orthant<N>,
    ) -> sled::Result<Vec<(ChunkKey<N>, Compr::Data)>> {
        let range = ChunkKey::<N>::orthant_range(lod, orthant);

        let mut chunks = Vec::new();

        for batch in &self.tree.range(range).chunks(16) {
            for batch_result in join_all(batch.into_iter().map(|kv_result| async move {
                let (key, compressed_chunk) = kv_result?;

                let ord_key = ChunkKey::<N>::ord_key_from_be_bytes(key.as_ref());
                let chunk_key = ChunkKey::<N>::from_ord_key(ord_key);

                let chunk = Compr::decompress_from_reader(compressed_chunk.as_ref()).unwrap();

                sled::Result::Ok((chunk_key, chunk))
            }))
            .await
            {
                let (chunk_key, chunk) = batch_result?;
                chunks.push((chunk_key, chunk));
            }
        }

        Ok(chunks)
    }
}

impl DatabaseKey<[i32; 2]> for ChunkKey2 {
    type Key = (u8, Morton2);

    // 1 for LOD and 8 for the morton code.
    type KeyBytes = [u8; 9];

    #[inline]
    fn into_ord_key(self) -> Self::Key {
        (self.lod, Morton2::from(self.minimum))
    }

    #[inline]
    fn from_ord_key((lod, morton): Self::Key) -> Self {
        ChunkKey::new(lod, Point2i::from(morton))
    }

    #[inline]
    fn ord_key_to_be_bytes((lod, morton): Self::Key) -> Self::KeyBytes {
        let mut bytes = [0; 9];
        bytes[0] = lod;
        bytes[1..].copy_from_slice(&morton.0.to_be_bytes());

        bytes
    }

    #[inline]
    fn ord_key_from_be_bytes(bytes: &[u8]) -> Self::Key {
        let lod = bytes[0];
        let mut morton_bytes = [0; 8];
        morton_bytes.copy_from_slice(&bytes[1..]);
        let morton_int = u64::from_be_bytes(morton_bytes);

        (lod, Morton2(morton_int))
    }

    #[inline]
    fn orthant_range(lod: u8, quad: Quadrant) -> RangeInclusive<Self::KeyBytes> {
        let extent = Extent2i::from(quad);
        let min_morton = Morton2::from(extent.minimum);
        let max_morton = Morton2::from(extent.max());
        let min_bytes = Self::ord_key_to_be_bytes((lod, min_morton));
        let max_bytes = Self::ord_key_to_be_bytes((lod, max_morton));

        min_bytes..=max_bytes
    }
}

impl DatabaseKey<[i32; 3]> for ChunkKey3 {
    type Key = (u8, Morton3);

    // 1 for LOD and 12 for the morton code. Although a `Morton3` uses a u128, it only actually uses the least significant 96
    // bits (12 bytes).
    type KeyBytes = [u8; 13];

    #[inline]
    fn into_ord_key(self) -> Self::Key {
        (self.lod, Morton3::from(self.minimum))
    }

    #[inline]
    fn from_ord_key((lod, morton): Self::Key) -> Self {
        ChunkKey::new(lod, Point3i::from(morton))
    }

    #[inline]
    fn ord_key_to_be_bytes((lod, morton): Self::Key) -> Self::KeyBytes {
        let mut bytes = [0; 13];
        bytes[0] = lod;
        bytes[1..].copy_from_slice(&morton.0.to_be_bytes()[4..]);

        bytes
    }

    #[inline]
    fn ord_key_from_be_bytes(bytes: &[u8]) -> Self::Key {
        let lod = bytes[0];
        // The most significant 4 bytes of the u128 are not used.
        let mut morton_bytes = [0; 16];
        morton_bytes[4..16].copy_from_slice(&bytes[1..]);
        let morton_int = u128::from_be_bytes(morton_bytes);

        (lod, Morton3(morton_int))
    }

    #[inline]
    fn orthant_range(lod: u8, octant: Octant) -> RangeInclusive<Self::KeyBytes> {
        let extent = Extent3i::from(octant);
        let min_morton = Morton3::from(extent.minimum);
        let max_morton = Morton3::from(extent.max());
        let min_bytes = Self::ord_key_to_be_bytes((lod, min_morton));
        let max_bytes = Self::ord_key_to_be_bytes((lod, max_morton));

        min_bytes..=max_bytes
    }
}

// ████████╗███████╗███████╗████████╗
// ╚══██╔══╝██╔════╝██╔════╝╚══██╔══╝
//    ██║   █████╗  ███████╗   ██║
//    ██║   ██╔══╝  ╚════██║   ██║
//    ██║   ███████╗███████║   ██║
//    ╚═╝   ╚══════╝╚══════╝   ╚═╝

#[cfg(test)]
mod test {
    use crate::{Array3x2, FastArrayCompressionNx2, FromBytesCompression, Lz4};

    use super::*;

    #[test]
    fn db_round_trip() -> sled::Result<()> {
        let chunk_mins = [
            PointN([16, 0, 0]),
            PointN([0, 16, 0]),
            PointN([0, 0, 16]),
            PointN([0, -16, 0]),
        ];
        let chunk_shape = Point3i::fill(16);
        let write_chunks: Vec<_> = chunk_mins
            .iter()
            .map(|&min| {
                (
                    ChunkKey3::new(0, min),
                    Array3x2::fill(Extent3i::from_min_and_shape(min, chunk_shape), (1u16, b'a')),
                )
            })
            .collect();

        let db = sled::Config::default()
            .path("/tmp/world1".to_owned())
            .use_compression(false)
            .mode(sled::Mode::LowSpace)
            .open()?;
        let tree = db.open_tree("chunks")?;

        // NOTE: This compression is not portable because it is naive to endianness.
        let compression = FastArrayCompressionNx2::from_bytes_compression(Lz4 { level: 10 });
        let chunk_db = ChunkDb::new(tree, compression);

        futures::executor::block_on(
            chunk_db.write_chunks(write_chunks.iter().map(|(k, v)| (*k, v))),
        )?;

        // This octant should contain the chunks in the positive octant, but not the other chunk.
        let octant = Octant::new_unchecked(Point3i::ZERO, 32);

        let read_chunks = futures::executor::block_on(chunk_db.read_chunks_in_orthant(0, octant))?;

        let read_keys: Vec<_> = read_chunks.iter().map(|(k, _)| k.clone()).collect();
        let expected_read_keys: Vec<_> =
            [PointN([16, 0, 0]), PointN([0, 16, 0]), PointN([0, 0, 16])]
                .iter()
                .cloned()
                .map(|min| ChunkKey3::new(0, min))
                .collect();
        assert_eq!(read_keys, expected_read_keys);

        assert_eq!(
            read_chunks,
            vec![
                write_chunks[0].clone(),
                write_chunks[1].clone(),
                write_chunks[2].clone()
            ]
        );

        Ok(())
    }
}
