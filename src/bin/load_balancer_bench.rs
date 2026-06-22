use distributed_neural_network::load_balancer::LoadBalancer;
use std::time::Instant;
use tokio::runtime::Runtime;
use uuid::Uuid;

fn main() {
    let rt = Runtime::new().expect("runtime");
    rt.block_on(async {
        let lb = LoadBalancer::new();
        for _ in 0..10_000 {
            lb.add_node(Uuid::new_v4()).await;
        }
        let start = Instant::now();
        for _ in 0..10_000 {
            lb.assign_task().await;
        }
        println!("Assigned 10k tasks in {:?}", start.elapsed());
    });
}
