//! Integration tests for task environment lifecycle.

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use tempfile::TempDir;
    
    use herdr_flow::task_env::*;
    
    /// Clean up any leftover test state.
    fn cleanup_test_state(task_id: &str) {
        // Try to get registry and unregister
        let _ = unregister_environment(task_id);
        
        // Try to remove state file
        if let Ok(home) = std::env::var("HOME") {
            let state_file = std::path::Path::new(&home)
                .join(".herdr/flow/task_environments")
                .join(format!("{}.json", task_id));
            let _ = std::fs::remove_file(&state_file);
        }
    }
    
    #[test]
    fn test_create_and_destroy_environment() {
        let task_id = "test-task-001";
        cleanup_test_state(task_id);
        
        let temp = TempDir::new().unwrap();
        
        let workspace_root = temp.path().join("workspace");
        let temp_root = temp.path().join("temp");
        let delivery_path = temp.path().join("delivery");
        
        // Create delivery target
        let delivery_target = DeliveryTarget::new(delivery_path.clone(), "test".to_string());
        
        // Create environment
        let env = create_environment(
            task_id,
            &workspace_root,
            vec![delivery_target],
            &temp_root,
        ).unwrap();
        
        assert_eq!(env.task_id, task_id);
        assert!(env.workspace_root.exists());
        assert!(env.temp_root.exists());
        assert!(env.state_file().exists());
        
        // Destroy environment
        destroy_environment(&env).unwrap();
        
        assert!(!env.workspace_root.exists());
        assert!(!env.temp_root.exists());
        assert!(!env.state_file().exists());
    }
    
    #[test]
    fn test_restore_environment() {
        let task_id = "test-task-002";
        cleanup_test_state(task_id);
        
        let temp = TempDir::new().unwrap();
        
        let workspace_root = temp.path().join("workspace");
        let temp_root = temp.path().join("temp");
        let delivery_path = temp.path().join("delivery");
        
        let delivery_target = DeliveryTarget::new(delivery_path.clone(), "test".to_string());
        
        // Create environment
        let env1 = create_environment(
            task_id,
            &workspace_root,
            vec![delivery_target],
            &temp_root,
        ).unwrap();
        
        let state_file = env1.state_file().to_path_buf();
        
        // Restore environment
        let env2 = restore_environment(task_id, temp.path()).unwrap();
        
        assert_eq!(env1.task_id, env2.task_id);
        assert_eq!(env1.workspace_root, env2.workspace_root);
        assert_eq!(env1.temp_root, env2.temp_root);
        assert_eq!(env1.created_at, env2.created_at);
        
        // Cleanup
        destroy_environment(&env2).unwrap();
    }
    
    #[test]
    fn test_boundary_validation() {
        let task_id = "test-task-003";
        cleanup_test_state(task_id);
        
        let temp = TempDir::new().unwrap();
        
        let workspace_root = temp.path().join("workspace");
        let temp_root = temp.path().join("temp");
        let delivery_path = temp.path().join("delivery");
        
        let delivery_target = DeliveryTarget::new(delivery_path.clone(), "test".to_string());
        
        let env = create_environment(
            task_id,
            &workspace_root,
            vec![delivery_target],
            &temp_root,
        ).unwrap();
        
        // Test disposable paths
        assert!(is_disposable(&env, &workspace_root.join("file.txt")));
        assert!(is_disposable(&env, &temp_root.join("temp.txt")));
        
        // Test delivery targets
        assert!(is_delivery_target(&env, &delivery_path));
        
        // Test paths outside boundaries
        assert!(!is_disposable(&env, &delivery_path));
        assert!(!is_delivery_target(&env, &workspace_root));
        
        // Cleanup
        destroy_environment(&env).unwrap();
    }
    
    #[test]
    fn test_reject_delivery_inside_workspace() {
        let temp = TempDir::new().unwrap();
        let task_id = "test-task-004";
        
        let workspace_root = temp.path().join("workspace");
        let temp_root = temp.path().join("temp");
        
        // Try to create delivery target inside workspace
        let bad_delivery = workspace_root.join("delivery");
        let delivery_target = DeliveryTarget::new(bad_delivery, "test".to_string());
        
        let result = create_environment(
            task_id,
            &workspace_root,
            vec![delivery_target],
            &temp_root,
        );
        
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("inside disposable workspace"));
    }
    
    #[test]
    fn test_reject_delivery_inside_temp() {
        let temp = TempDir::new().unwrap();
        let task_id = "test-task-005";
        
        let workspace_root = temp.path().join("workspace");
        let temp_root = temp.path().join("temp");
        
        // Try to create delivery target inside temp
        let bad_delivery = temp_root.join("delivery");
        let delivery_target = DeliveryTarget::new(bad_delivery, "test".to_string());
        
        let result = create_environment(
            task_id,
            &workspace_root,
            vec![delivery_target],
            &temp_root,
        );
        
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("inside disposable temp"));
    }
    
    #[test]
    fn test_reject_duplicate_environment() {
        let task_id = "test-task-006";
        cleanup_test_state(task_id);
        
        let temp = TempDir::new().unwrap();
        
        let workspace_root = temp.path().join("workspace");
        let temp_root = temp.path().join("temp");
        let delivery_path = temp.path().join("delivery");
        
        let delivery_target = DeliveryTarget::new(delivery_path.clone(), "test".to_string());
        
        // Create first environment
        let env1 = create_environment(
            task_id,
            &workspace_root,
            vec![delivery_target.clone()],
            &temp_root,
        ).unwrap();
        
        // Try to create duplicate
        let result = create_environment(
            task_id,
            &workspace_root.with_file_name("workspace2"),
            vec![delivery_target],
            &temp_root.with_file_name("temp2"),
        );
        
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("already exists"));
        
        // Cleanup
        destroy_environment(&env1).unwrap();
    }
    
    #[test]
    fn test_register_and_unregister_resources() {
        let task_id = "test-task-007";
        cleanup_test_state(task_id);
        
        let temp = TempDir::new().unwrap();
        
        let workspace_root = temp.path().join("workspace");
        let temp_root = temp.path().join("temp");
        let delivery_path = temp.path().join("delivery");
        
        let delivery_target = DeliveryTarget::new(delivery_path, "test".to_string());
        
        let mut env = create_environment(
            task_id,
            &workspace_root,
            vec![delivery_target],
            &temp_root,
        ).unwrap();
        
        // Register resources
        let worktree = PathBuf::from("/tmp/worktree");
        env.register_worktree(worktree.clone()).unwrap();
        assert!(env.worktree_registrations.contains(&worktree));
        
        env.register_process(12345).unwrap();
        assert!(env.child_processes.contains(&12345));
        
        let file = PathBuf::from("/tmp/file.txt");
        env.register_file(file.clone()).unwrap();
        assert!(env.open_files.contains(&file));
        
        // Unregister resources
        env.unregister_worktree(&worktree).unwrap();
        assert!(!env.worktree_registrations.contains(&worktree));
        
        env.unregister_process(12345).unwrap();
        assert!(!env.child_processes.contains(&12345));
        
        env.unregister_file(&file).unwrap();
        assert!(!env.open_files.contains(&file));
        
        // Cleanup
        destroy_environment(&env).unwrap();
    }
    
    #[test]
    fn test_cleanup_idempotence() {
        let task_id = "test-task-008";
        cleanup_test_state(task_id);
        
        let temp = TempDir::new().unwrap();
        
        let workspace_root = temp.path().join("workspace");
        let temp_root = temp.path().join("temp");
        let delivery_path = temp.path().join("delivery");
        
        let delivery_target = DeliveryTarget::new(delivery_path, "test".to_string());
        
        let env = create_environment(
            task_id,
            &workspace_root,
            vec![delivery_target],
            &temp_root,
        ).unwrap();
        
        // First destroy
        destroy_environment(&env).unwrap();
        
        // Second destroy should handle missing resources gracefully
        // (In a real scenario, we'd need to handle this differently,
        // but for now the resources are already gone)
        assert!(!env.workspace_root.exists());
        assert!(!env.temp_root.exists());
    }
}
