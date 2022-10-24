use criterion::{
    async_executor::AsyncStdExecutor, black_box, criterion_group, criterion_main, Criterion,
    Throughput,
};
use std::rc::Rc;
use wnfs::{
    dagcbor,
    private::hamt::{Hamt, Node},
    BlockStore, MemoryBlockStore,
};

fn node_set(c: &mut Criterion) {
    c.bench_function("Node set", |b| {
        b.to_async(AsyncStdExecutor).iter(|| async {
            let store = &mut MemoryBlockStore::default();
            let mut node = black_box(Rc::new(<Node<_, _>>::default()));

            for i in 0..50 {
                node = black_box(node.set(i.to_string(), i, store).await.unwrap());
            }
        })
    });
}

fn node_load_get(c: &mut Criterion) {
    let mut store = MemoryBlockStore::default();
    let (cid, bytes) = async_std::task::block_on(async {
        let mut node = Rc::new(<Node<_, _>>::default());
        for i in 0..50 {
            node = node.set(i.to_string(), i, &mut store).await.unwrap();
        }

        let encoded_hamt = dagcbor::async_encode(&Hamt::with_root(node), &mut store)
            .await
            .unwrap();

        let cid = store.put_serializable(&encoded_hamt).await.unwrap();

        (cid, encoded_hamt)
    });

    let mut group = c.benchmark_group("With throughput");
    group.throughput(Throughput::Bytes(bytes.len() as u64));
    group.bench_function("Node load and get", |b| {
        b.to_async(AsyncStdExecutor).iter(|| async {
            let encoded_hamt = store.get_deserializable::<Vec<u8>>(&cid).await.unwrap();
            let hamt: Hamt<String, i32> = dagcbor::decode(encoded_hamt.as_ref()).unwrap();

            for i in 0..50 {
                assert!(hamt
                    .root
                    .get(&i.to_string(), &store)
                    .await
                    .unwrap()
                    .is_some());
            }
        })
    });

    group.finish();
}

fn node_load_remove(c: &mut Criterion) {
    let mut store = MemoryBlockStore::default();
    let (cid, bytes) = async_std::task::block_on(async {
        let mut node = Rc::new(<Node<_, _>>::default());
        for i in 0..50 {
            node = node.set(i.to_string(), i, &mut store).await.unwrap();
        }

        let encoded_hamt = dagcbor::async_encode(&Hamt::with_root(node), &mut store)
            .await
            .unwrap();

        let cid = store.put_serializable(&encoded_hamt).await.unwrap();

        (cid, encoded_hamt)
    });

    let mut group = c.benchmark_group("With throughput");
    group.throughput(Throughput::Bytes(bytes.len() as u64));
    group.bench_function("Node load and remove", |b| {
        b.to_async(AsyncStdExecutor).iter(|| async {
            let encoded_hamt = store.get_deserializable::<Vec<u8>>(&cid).await.unwrap();
            let mut hamt: Hamt<String, i32> =
                black_box(dagcbor::decode(encoded_hamt.as_ref()).unwrap());

            for i in 0..50 {
                let (root, value) = hamt.root.remove(&i.to_string(), &store).await.unwrap();
                assert!(value.is_some());
                hamt.root = root;
            }
        })
    });
}

fn hamt_load_decode(c: &mut Criterion) {
    let mut store = MemoryBlockStore::default();
    let (cid, bytes) = async_std::task::block_on(async {
        let mut node = Rc::new(<Node<_, _>>::default());
        for i in 0..50 {
            node = node.set(i.to_string(), i, &mut store).await.unwrap();
        }

        let encoded_hamt = dagcbor::async_encode(&Hamt::with_root(node), &mut store)
            .await
            .unwrap();

        let cid = store.put_serializable(&encoded_hamt).await.unwrap();

        (cid, encoded_hamt)
    });

    let mut group = c.benchmark_group("With throughput");
    group.throughput(Throughput::Bytes(bytes.len() as u64));
    group.bench_function("HAMT load and decode", |b| {
        b.to_async(AsyncStdExecutor).iter(|| async {
            let encoded_hamt = store.get_deserializable::<Vec<u8>>(&cid).await.unwrap();
            let _: Hamt<String, i32> = black_box(dagcbor::decode(encoded_hamt.as_ref()).unwrap());
        })
    });
}

fn hamt_set_encode(c: &mut Criterion) {
    c.bench_function("HAMT set and encode", |b| {
        b.to_async(AsyncStdExecutor).iter(|| async {
            let store = &mut MemoryBlockStore::default();
            let mut node = Rc::new(<Node<_, _>>::default());
            for i in 0..50 {
                node = node.set(i.to_string(), i, store).await.unwrap();
            }

            let hamt = Hamt::with_root(node);

            let _ = black_box(dagcbor::async_encode(&hamt, store).await.unwrap());
        })
    });
}

criterion_group!(
    benches,
    node_set,
    node_load_get,
    node_load_remove,
    hamt_load_decode,
    hamt_set_encode
);

criterion_main!(benches);
