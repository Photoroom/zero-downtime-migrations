# R001: Non-concurrent AddIndex - should fail
from django.db import migrations, models


class Migration(migrations.Migration):

    dependencies = [
        ('myapp', '0001_initial'),
    ]

    operations = [
        migrations.AddIndex(
            model_name='order',
            index=models.Index(fields=['created_at'], name='order_created_idx'),
        ),
    ]
